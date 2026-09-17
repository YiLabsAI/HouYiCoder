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
fn test_transcript_shows_redundant_calls() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(2),
        response: ServerResponse::Trajectory {
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
        },
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
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(tool_call_frame(
        "call_1",
        "bash",
        ToolCallStatus::InProgress,
    ))));
    assert!(app.running_tools.contains("call_1"));
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(tool_done_frame(
        "call_1",
    ))));
    assert!(app.running_tools.is_empty());
}

#[test]
fn test_done_clears_running_tools() {
    let mut app = crate::composition::app();
    app.start_run_for_test(1);
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(tool_call_frame(
        "call_1",
        "bash",
        ToolCallStatus::InProgress,
    ))));
    app.handle_agent_message(done_msg());
    assert!(app.running_tools.is_empty());
}

#[test]
fn test_completed_list_timestamped() {
    // A completed item in a live run gets a timestamp; an in-progress item
    // does not. The distinction drives the footer grace window.
    let mut app = crate::composition::app();
    app.start_run_for_test(1);
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(todo_frame(&[
        ("old work", "completed"),
        ("current", "in_progress"),
    ]))));
    app.handle_agent_message(done_msg());
    assert!(
        app.todos.completion_at.contains_key("old work"),
        "completed item timestamped: {:?}",
        app.todos.completion_at
    );
    assert!(
        !app.todos.completion_at.contains_key("current"),
        "in-progress item not timestamped"
    );
    // Completing the whole list timestamps every item for one shared
    // clearing.
    app.start_run_for_test(1);
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(todo_frame(&[
        ("old work", "completed"),
        ("current", "completed"),
    ]))));
    app.handle_agent_message(done_msg());
    assert!(app.todos.completion_at.contains_key("current"));
    assert!(app.todos.completion_at.contains_key("old work"));
}

/// Replaying a session whose latest task list is all-completed, what a
/// resume does, clears the list on the spot: no items install, no
/// timestamps record, and the rendered terminal never shows the historic
/// tasks. Every repeat attach behaves the same way.
#[test]
fn test_replayed_done_clears() {
    for _ in 0..3 {
        let mut app = crate::composition::app();
        app.screen = crate::state::Screen::Working;
        app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(todo_frame(&[
            ("old work", "completed"),
            ("current", "completed"),
        ]))));
        app.start_run_for_test(1);
        app.handle_agent_message(done_msg());
        assert!(app.todos.items.is_empty());
        assert!(app.todos.completion_at.is_empty());
        let out = crate::test_harness::render_text(&app, 100, 24);
        assert!(
            !out.contains("old work"),
            "historic tasks must not render after a resume: {out}"
        );
    }
}

/// The other half of the resume matrix: a replayed list with open work
/// still renders, so clearing targets finished history only.
#[test]
fn test_replayed_open_renders() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(todo_frame(&[
        ("old work", "completed"),
        ("open task", "pending"),
    ]))));
    app.start_run_for_test(1);
    app.handle_agent_message(done_msg());
    let out = crate::test_harness::render_text(&app, 100, 24);
    assert!(
        out.contains("open task"),
        "resumed open work must render: {out}"
    );
}

/// A toggle-state result updates memory state without changing the active pane.
#[test]
fn test_toggle_state_applies_view() {
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::memory::ToggleState;
    // On the pane: the snapshot applies + the pane stays open.
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Memory;
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(1),
        response: ServerResponse::MemoryToggleState {
            state: ToggleState {
                auto_memory: false,
                auto_dream: true,
            },
        },
    });
    assert!(!app.memory.toggles().auto_memory, "auto-memory applied");
    assert!(app.memory.toggles().auto_dream, "auto-dream applied");
    assert_eq!(app.pane, crate::state::Pane::Memory, "pane stays open");
    // Dismissed (pane moved away): the snapshot still applies, but the
    // pane is NOT yanked back to Memory.
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Spec;
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(2),
        response: ServerResponse::MemoryToggleState {
            state: ToggleState {
                auto_memory: false,
                auto_dream: true,
            },
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
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::memory::MemorySummaryEntry;
    // On the pane: the list populates + the pane stays open.
    let mut app = crate::composition::app();
    app.pane = Pane::Memory;
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(1),
        response: ServerResponse::MemoryList {
            entries: vec![MemorySummaryEntry {
                key: "build-gate".to_string(),
                description: "make check stays green".to_string(),
                source: "project".to_string(),
                scope: "project".to_string(),
                mtime_secs: 0,
            }],
        },
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
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(2),
        response: ServerResponse::MemoryList {
            entries: vec![MemorySummaryEntry {
                key: "build-gate".to_string(),
                description: "make check stays green".to_string(),
                source: "project".to_string(),
                scope: "project".to_string(),
                mtime_secs: 0,
            }],
        },
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
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(9),
        response: ServerResponse::MemoryShow {
            entry: Some(MemoryDetail {
                key: "build-gate".into(),
                content: "make check stays green".into(),
                source: "project".into(),
                description: "verification".into(),
                mtime_secs: 0,
            }),
        },
    });
    assert!(app.transcript.iter().any(|line| matches!(
        line,
        crate::records::TranscriptLine::System(text) if text.contains("build-gate")
    )));
}

#[test]
fn test_notice_shows_memory_changes() {
    use crate::records::TranscriptLine;
    use crate::state::Screen;
    use houyicoder_protocol::frontend::memory::{
        MemoryChange, MemoryChangeId, MemoryChangeOrigin, MemoryOperation,
    };
    let mut app = crate::composition::app();
    app.screen = Screen::Working;
    let changes = vec![
        MemoryChange {
            key: "alpha".into(),
            operation: MemoryOperation::Promoted,
        },
        MemoryChange {
            key: "beta".into(),
            operation: MemoryOperation::Deleted,
        },
    ];
    app.handle_agent_message(SessionMessage::Event(ServerEvent::MemoryChanged {
        id: MemoryChangeId("change-1".into()),
        origin: MemoryChangeOrigin::AutoDream,
        changes: changes.clone(),
    }));
    app.handle_agent_message(SessionMessage::Event(ServerEvent::MemoryChanged {
        id: MemoryChangeId("change-1".into()),
        origin: MemoryChangeOrigin::AutoDream,
        changes,
    }));
    assert_eq!(
        app.transcript
            .iter()
            .filter(
                |line| matches!(line, TranscriptLine::System(text) if text.contains("auto-dream"))
            )
            .count(),
        1
    );
    let out = crate::test_harness::render_text(&app, 100, 24);
    assert!(out.contains("auto-dream"), "origin should render: {out}");
    assert!(
        out.contains("promoted alpha"),
        "promotion should render: {out}"
    );
    assert!(
        out.contains("deleted beta"),
        "deletion should render: {out}"
    );
    assert!(
        out.contains("Memory auto-dream: 2 changes · /memory"),
        "the summary must not inline every key: {out}"
    );
    assert!(
        out.contains("⎿  promoted alpha") && out.contains("⎿  deleted beta"),
        "each exact change remains visible on a child row: {out}"
    );
}

#[test]
fn test_notice_wraps_long_keys() {
    use houyicoder_protocol::frontend::memory::{
        MemoryChange, MemoryChangeId, MemoryChangeOrigin, MemoryOperation,
    };
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    let changes = (0..4)
        .map(|index| MemoryChange {
            key: format!("project-memory-with-a-deliberately-long-key-{index}"),
            operation: MemoryOperation::Stored,
        })
        .collect();
    app.handle_agent_message(SessionMessage::Event(ServerEvent::MemoryChanged {
        id: MemoryChangeId("change-long".into()),
        origin: MemoryChangeOrigin::AutoMemory,
        changes,
    }));
    let out = crate::test_harness::render_text(&app, 54, 36);
    assert!(
        out.contains("Memory auto-memory: 4 changes · /memory"),
        "summary remains visible after wrapping: {out}"
    );
    assert_eq!(
        out.matches('⎿').count(),
        4,
        "every change keeps its own visible child row after wrapping: {out}"
    );
}
