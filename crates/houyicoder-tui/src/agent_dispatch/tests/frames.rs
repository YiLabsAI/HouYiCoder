use super::*;

#[test]
fn test_frame_log_call_id() {
    let f = tool_call_frame("c1", "glob", ToolCallStatus::InProgress);
    let msg = super::frame_log_msg(&f).expect("call frame logged");
    assert!(msg.contains("id=c1"), "id in {msg}");
    assert!(msg.contains("tool=glob"), "tool in {msg}");
}

#[test]
fn test_frame_log_result_shape() {
    // A diff-bearing result (edit) tags "diff"; a glob result tags "files";
    // a status-only update tags "status". These tags let a debug log reveal
    // a swapped call_id at the server or a TUI pairing bug at a glance.
    let diff = TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "c1",
        ToolCallUpdateFields::new()
            .raw_output(serde_json::json!({"diff": "@@ -1 +1 @@\n-a\n+b\n"})),
    )));
    let files = TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "c2",
        ToolCallUpdateFields::new().raw_output(serde_json::json!({"num_files": 0})),
    )));
    let status = tool_done_frame("c3");
    assert!(
        super::frame_log_msg(&diff).unwrap().contains("shape=diff"),
        "diff result tags diff"
    );
    assert!(
        super::frame_log_msg(&files)
            .unwrap()
            .contains("shape=files"),
        "glob result tags files"
    );
    assert!(
        super::frame_log_msg(&status)
            .unwrap()
            .contains("shape=status"),
        "status-only tags status"
    );
}

/// A TrajectoryResult with redundant calls renders the redundant section
/// (same-message repeat / cross-turn context-loss re-read) as a system
/// line.
#[test]
fn test_trajectory_renders_redundant_section() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.handle_agent_message(AgentMessage::TrajectoryResult {
        entries: vec![],
        redundant: vec![
            houyicoder_protocol::frontend::trajectory::RedundantCallEntry {
                tool: "read".into(),
                input_preview: "{\"file_path\":\"a.rs\"}".into(),
                kind: "same-batch".into(),
                gap: 0,
                last_seq: 3,
            },
        ],
    });
    assert!(
        app.transcript.iter().any(|l| matches!(
            l,
            TranscriptLine::System(s) if s.contains("same-message repeat")
        )),
        "redundant section rendered"
    );
}

#[test]
fn test_tool_frames_track_set() {
    let mut app = crate::composition::app();
    app.handle_agent_message(AgentMessage::Frame(tool_call_frame(
        "call_1",
        "bash",
        ToolCallStatus::InProgress,
    )));
    assert!(app.running_tools.contains("call_1"));
    app.handle_agent_message(AgentMessage::Frame(tool_done_frame("call_1")));
    assert!(app.running_tools.is_empty());
}

#[test]
fn test_done_clears_running_tools() {
    let mut app = crate::composition::app();
    app.handle_agent_message(AgentMessage::Frame(tool_call_frame(
        "call_1",
        "bash",
        ToolCallStatus::InProgress,
    )));
    app.handle_agent_message(done_msg());
    assert!(app.running_tools.is_empty());
}

#[test]
fn test_completed_cohort_stamped() {
    // Historic completions are not recent while unresolved work remains.
    let mut app = crate::composition::app();
    app.handle_agent_message(AgentMessage::Frame(todo_frame(&[
        ("old work", "completed"),
        ("current", "in_progress"),
    ])));
    app.handle_agent_message(done_msg());
    assert!(app.todos.completion_at.is_empty());
    // Completing the cohort timestamps every item for one shared retirement.
    app.handle_agent_message(AgentMessage::Frame(todo_frame(&[
        ("old work", "completed"),
        ("current", "completed"),
    ])));
    app.handle_agent_message(done_msg());
    assert!(app.todos.completion_at.contains_key("current"));
    assert!(app.todos.completion_at.contains_key("old work"));
}

/// A toggle-state result updates memory state without changing the active pane.
#[test]
fn test_toggle_state_applies_view() {
    use houyicoder_protocol::frontend::memory::ToggleState;
    // On the pane: the snapshot applies + the pane stays open.
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Memory;
    app.handle_agent_message(AgentMessage::MemoryToggleStateResult {
        state: ToggleState {
            auto_memory: false,
            auto_dream: true,
        },
    });
    assert!(!app.memory.toggles().auto_memory, "auto-memory applied");
    assert!(app.memory.toggles().auto_dream, "auto-dream applied");
    assert_eq!(app.pane, crate::state::Pane::Memory, "pane stays open");
    // Dismissed (pane moved away): the snapshot still applies, but the
    // pane is NOT yanked back to Memory.
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Spec;
    app.handle_agent_message(AgentMessage::MemoryToggleStateResult {
        state: ToggleState {
            auto_memory: false,
            auto_dream: true,
        },
    });
    assert!(
        !app.memory.toggles().auto_memory,
        "auto-memory applied on dismissal"
    );
    assert_eq!(
        app.pane,
        crate::state::Pane::Spec,
        "late result does not yank back a dismissed pane"
    );
}

/// A list refresh updates memory data without changing the active pane.
#[test]
fn test_memory_list_respects_dismissal() {
    use houyicoder_protocol::frontend::memory::MemorySummaryEntry;
    // On the pane: the list populates + the pane stays open.
    let mut app = crate::composition::app();
    app.pane = Pane::Memory;
    app.handle_agent_message(AgentMessage::MemoryListResult {
        entries: vec![MemorySummaryEntry {
            key: "build-gate".to_string(),
            description: "make check stays green".to_string(),
            source: "project".to_string(),
            scope: "project".to_string(),
            mtime_secs: 0,
        }],
    });
    assert_eq!(app.pane, Pane::Memory, "pane stays open on active refresh");
    assert!(
        app.memory.entries().iter().any(|e| e.topic == "build-gate"),
        "list entry populated"
    );
    // Dismissed (pane moved away): the data still lands, but the pane is
    // NOT yanked back to Memory.
    let mut app = crate::composition::app();
    app.pane = Pane::Spec;
    app.handle_agent_message(AgentMessage::MemoryListResult {
        entries: vec![MemorySummaryEntry {
            key: "build-gate".to_string(),
            description: "make check stays green".to_string(),
            source: "project".to_string(),
            scope: "project".to_string(),
            mtime_secs: 0,
        }],
    });
    assert_eq!(
        app.pane,
        Pane::Spec,
        "late list does not yank back a dismissed pane"
    );
    assert!(
        app.memory.entries().iter().any(|e| e.topic == "build-gate"),
        "list data still lands on dismissal"
    );
}

#[test]
fn test_pane_shows_command_result() {
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::memory::MemoryDetail;

    let mut app = crate::composition::app();
    app.pane = Pane::Memory;
    app.handle_agent_message(AgentMessage::MemoryShowResult {
        req_id: RequestId(9),
        entry: Some(MemoryDetail {
            key: "build-gate".into(),
            content: "make check stays green".into(),
            source: "project".into(),
            description: "verification".into(),
            mtime_secs: 0,
        }),
    });
    assert!(app.transcript.iter().any(|line| matches!(
        line,
        crate::records::TranscriptLine::System(text) if text.contains("build-gate")
    )));
}

/// Equal adjacent memory events render one notice.
#[test]
fn test_memory_notice_once() {
    use crate::state::{Screen, TranscriptLine};
    use houyicoder_protocol::frontend::memory::MemorySavedKind;
    let mut app = crate::composition::app();
    app.screen = Screen::Working;

    // Extracted, plural: "Saved 3 memories".
    app.handle_agent_message(AgentMessage::MemorySaved {
        count: 3,
        kind: MemorySavedKind::Extracted,
    });
    app.handle_agent_message(AgentMessage::MemorySaved {
        count: 3,
        kind: MemorySavedKind::Extracted,
    });
    let notices = app
        .transcript
        .iter()
        .filter(|line| {
            matches!(line, TranscriptLine::System(text) if text.starts_with("Saved 3 memories"))
        })
        .count();
    assert_eq!(notices, 1);
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("Saved 3 memories"),
        "extract verb + plural noun should render: {out}"
    );

    // Consolidation reports touches, not resulting entry count.
    app.handle_agent_message(AgentMessage::MemorySaved {
        count: 1,
        kind: MemorySavedKind::Consolidated,
    });
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("Consolidated 1 memory change"),
        "consolidation touch count should render: {out}"
    );
}
