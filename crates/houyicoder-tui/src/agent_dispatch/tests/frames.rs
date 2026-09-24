use super::*;

use houyicoder_protocol::frontend::memory::{
    MemoryChange, MemoryChangeCausality, MemoryChangeId, MemoryChangeOrigin, MemoryChangeScope,
    MemoryOperation,
};

use crate::composition;
use crate::keys::handle_ctrl_o;
use crate::records::TranscriptLine;
use crate::state::{App, Pane, Screen};
use crate::test_harness::render_text;
use crate::view::line_wrap::wrap_line;

/// A working-screen app holding one memory-change notice: the shape every
/// notice test starts from, so the broadcast setup lives here once.
fn app_with_notice(
    id: &str,
    causality: MemoryChangeCausality,
    keys: &[(&str, MemoryOperation)],
) -> App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.handle_agent_message(SessionMessage::Event(ServerEvent::MemoryChanged {
        id: MemoryChangeId(id.into()),
        origin: MemoryChangeOrigin::AutoMemory,
        causality,
        changes: keys
            .iter()
            .map(|(key, operation)| MemoryChange {
                key: (*key).to_string(),
                operation: *operation,
                scope: MemoryChangeScope::Auto,
            })
            .collect(),
    }));
    app
}

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
    let mut app = composition::app();
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
    let mut app = composition::app();
    app.start_run_for_test(0);
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(tool_call_frame(
        "call_1",
        "bash",
        ToolCallStatus::InProgress,
    ))));
    assert!(
        app.run_progress()
            .is_some_and(|p| p.running_tools.contains("call_1"))
    );
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(tool_done_frame(
        "call_1",
    ))));
    assert!(
        app.run_progress()
            .expect("active run")
            .running_tools
            .is_empty()
    );
}

#[test]
fn test_done_clears_running_tools() {
    let mut app = composition::app();
    app.start_run_for_test(1);
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(tool_call_frame(
        "call_1",
        "bash",
        ToolCallStatus::InProgress,
    ))));
    app.handle_agent_message(done_msg());
    assert!(
        app.run_progress()
            .is_none_or(|p| p.running_tools.is_empty())
    );
}

/// A streaming delta racing the Done reply lands after finish drops the run,
/// so it must not resurrect a live preview. This is the one deliberate
/// behavior change of the RunProgress move: the old flat App fields would
/// have swallowed the late delta into a stale phantom preview.
#[test]
fn test_late_delta_dropped() {
    let mut app = composition::app();
    app.start_run_for_test(1);
    app.handle_agent_message(done_msg());
    assert!(app.run_progress().is_none(), "done drops the run");
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Delta {
        text: "late".into(),
    }));
    assert!(
        app.run_progress()
            .is_none_or(|p| p.live_assistant_text.is_empty()),
        "a late delta after done must not resurrect a live preview"
    );
}

#[test]
fn test_completed_list_timestamped() {
    // A completed item in a live run gets a timestamp; an in-progress item
    // does not. The distinction drives the footer grace window.
    let mut app = composition::app();
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
        let mut app = composition::app();
        app.screen = Screen::Working;
        app.todos.set_replaying_history(true);
        app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(todo_frame(&[
            ("old work", "completed"),
            ("current", "completed"),
        ]))));
        app.start_run_for_test(1);
        app.handle_agent_message(done_msg());
        assert!(app.todos.items.is_empty());
        assert!(app.todos.completion_at.is_empty());
        let out = render_text(&app, 100, 24);
        assert!(
            !out.contains("old work"),
            "historic tasks must not render after a resume: {out}"
        );
    }
}

/// The regression the one-shot cold proxy missed: a replayed transcript that
/// reaches all-completed on a LATER frame (after a partial list already
/// installed) still clears on the spot, so the rendered footer never flashes
/// the finished list. Each frame arrives as its own update, mirroring a
/// resume re-feeding history frame by frame.
#[test]
fn test_replayed_later_frame_clears() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.todos.set_replaying_history(true);
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(todo_frame(&[
        ("one", "in_progress"),
        ("two", "pending"),
    ]))));
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(todo_frame(&[
        ("one", "completed"),
        ("two", "pending"),
    ]))));
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(todo_frame(&[
        ("one", "completed"),
        ("two", "completed"),
    ]))));
    app.start_run_for_test(1);
    app.handle_agent_message(done_msg());
    assert!(app.todos.items.is_empty());
    assert!(app.todos.completion_at.is_empty());
    let out = render_text(&app, 100, 24);
    assert!(
        !out.contains("one") && !out.contains("two"),
        "historic tasks must not flash after a multi-frame resume: {out}"
    );
}

/// The other half of the resume matrix: a replayed list with open work
/// still renders, so clearing targets finished history only.
#[test]
fn test_replayed_open_renders() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.todos.set_replaying_history(true);
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(todo_frame(&[
        ("old work", "completed"),
        ("open task", "pending"),
    ]))));
    app.start_run_for_test(1);
    app.handle_agent_message(done_msg());
    let out = render_text(&app, 100, 24);
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
    let mut app = composition::app();
    app.pane = Pane::Memory;
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
    assert_eq!(app.pane, Pane::Memory, "pane stays open");
    // Dismissed (pane moved away): the snapshot still applies, but the
    // pane is NOT yanked back to Memory.
    let mut app = composition::app();
    app.pane = Pane::Spec;
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
        Pane::Spec,
        "late result does not yank back a dismissed pane"
    );
}

/// A list refresh updates memory data without changing the active pane.
#[test]
fn test_memory_list_respects_dismissal() {
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::memory::MemorySummaryEntry;
    // On the pane: the list populates + the pane stays open.
    let mut app = composition::app();
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
    let mut app = composition::app();
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

    let mut app = composition::app();
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
        TranscriptLine::System(text) if text.contains("build-gate")
    )));
}

#[test]
fn test_notice_shows_memory_changes() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    let changes = vec![
        MemoryChange {
            key: "alpha".into(),
            operation: MemoryOperation::Promoted,
            scope: MemoryChangeScope::Project,
        },
        MemoryChange {
            key: "beta".into(),
            operation: MemoryOperation::Deleted,
            scope: MemoryChangeScope::Auto,
        },
    ];
    app.handle_agent_message(SessionMessage::Event(ServerEvent::MemoryChanged {
        id: MemoryChangeId("change-1".into()),
        origin: MemoryChangeOrigin::PrimaryAgent,
        causality: MemoryChangeCausality::PreviousTurn,
        changes: changes.clone(),
    }));
    app.handle_agent_message(SessionMessage::Event(ServerEvent::MemoryChanged {
        id: MemoryChangeId("change-1".into()),
        origin: MemoryChangeOrigin::PrimaryAgent,
        causality: MemoryChangeCausality::PreviousTurn,
        changes,
    }));
    assert_eq!(
        app.transcript
            .iter()
            .filter(|line| matches!(line, TranscriptLine::System(text) if text.contains("Memory")))
            .count(),
        1
    );
    let out = render_text(&app, 100, 24);
    assert!(
        out.contains("Memory saved by the agent: 2 changes · /memory"),
        "a batch of mixed operations takes the producer label: {out}"
    );
    assert!(
        !out.contains("auto-dream") && !out.contains("primary agent"),
        "the producer is not an object name in the notice: {out}"
    );
    assert!(
        !out.contains("⎿  promoted alpha") && !out.contains("⎿  deleted beta"),
        "several changes collapse to the summary, the keys open on expand: {out}"
    );
    // A second, single-change notice: its key is also collapsed by default,
    // and reveals when the notice is expanded (mg#1 = the second notice).
    let single = vec![MemoryChange {
        key: "gamma".into(),
        operation: MemoryOperation::Created,
        scope: MemoryChangeScope::Auto,
    }];
    app.handle_agent_message(SessionMessage::Event(ServerEvent::MemoryChanged {
        id: MemoryChangeId("change-2".into()),
        origin: MemoryChangeOrigin::PrimaryAgent,
        causality: MemoryChangeCausality::ThisTurn,
        changes: single,
    }));
    let out = render_text(&app, 100, 24);
    assert!(
        !out.contains("⎿  created gamma"),
        "a second single-change notice is collapsed too: {out}"
    );
    app.expanded_fold_groups.insert("mg#1".into());
    let out = render_text(&app, 100, 24);
    assert!(
        out.contains("⎿  created gamma"),
        "expanding the notice reveals its key: {out}"
    );
}

#[test]
fn test_unknown_causality_reads_earlier() {
    // A causality this build does not name must never claim the turn in
    // front of the user.
    let app = app_with_notice(
        "change-u",
        MemoryChangeCausality::Unknown,
        &[("alpha", MemoryOperation::Updated)],
    );
    let out = render_text(&app, 100, 24);
    assert!(
        out.contains("Memory extracted in background: 1 change · /memory"),
        "an unrecognized causality reads as an earlier turn: {out}"
    );
}

/// The expanded detail names the scope each change was addressed to, so the
/// notice says where a memory went and not only which key moved.
#[test]
fn test_notice_detail_names_scope() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.handle_agent_message(SessionMessage::Event(ServerEvent::MemoryChanged {
        id: MemoryChangeId("change-scope".into()),
        origin: MemoryChangeOrigin::PrimaryAgent,
        causality: MemoryChangeCausality::ThisTurn,
        changes: vec![
            MemoryChange {
                key: "alpha".into(),
                operation: MemoryOperation::Promoted,
                scope: MemoryChangeScope::Project,
            },
            MemoryChange {
                key: "beta".into(),
                operation: MemoryOperation::Promoted,
                scope: MemoryChangeScope::User,
            },
        ],
    }));
    app.expanded_fold_groups.insert("mg#0".into());
    let out = render_text(&app, 100, 24);
    assert!(
        out.contains("⎿  promoted alpha · scope: project")
            && out.contains("⎿  promoted beta · scope: user"),
        "each detail row names the scope the change was addressed to: {out}"
    );
}

/// A frame that never named a scope leaves the token off the row instead of
/// printing one the producer did not claim.
#[test]
fn test_notice_skips_unknown_scope() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.handle_agent_message(SessionMessage::Event(ServerEvent::MemoryChanged {
        id: MemoryChangeId("change-noscope".into()),
        origin: MemoryChangeOrigin::PrimaryAgent,
        causality: MemoryChangeCausality::ThisTurn,
        changes: vec![MemoryChange {
            key: "alpha".into(),
            operation: MemoryOperation::Created,
            scope: MemoryChangeScope::Unknown,
        }],
    }));
    app.expanded_fold_groups.insert("mg#0".into());
    let out = render_text(&app, 100, 24);
    assert!(
        out.contains("⎿  created alpha"),
        "the row still names the key and the operation: {out}"
    );
    assert!(
        !out.contains("alpha ·") && !out.contains(" · unknown"),
        "an unnamed root is left off the row: {out}"
    );
}

#[test]
fn test_notice_summarizes_many_changes() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    let changes = (0..4)
        .map(|index| MemoryChange {
            key: format!("project-memory-with-a-deliberately-long-key-{index}"),
            operation: MemoryOperation::Created,
            scope: MemoryChangeScope::Auto,
        })
        .collect();
    app.handle_agent_message(SessionMessage::Event(ServerEvent::MemoryChanged {
        id: MemoryChangeId("change-long".into()),
        origin: MemoryChangeOrigin::PrimaryAgent,
        causality: MemoryChangeCausality::ThisTurn,
        changes,
    }));
    let out = render_text(&app, 54, 36);
    assert!(
        out.contains("Memory saved by the agent: 4 changes · /memory"),
        "a batch sharing one operation takes its verb: {out}"
    );
    assert_eq!(
        out.matches('⎿').count(),
        0,
        "several changes collapse to the summary, no key echoed: {out}"
    );
    // Expanding the notice reveals each change as its own ⎿ row (the long keys
    // hard-break across rows, so the per-key rows are the stable assertion).
    app.expanded_fold_groups.insert("mg#0".into());
    let out = render_text(&app, 54, 36);
    assert_eq!(
        out.matches('⎿').count(),
        4,
        "each change is a ⎿ row when the notice is expanded: {out}"
    );
}

#[test]
fn test_notice_single_change_wraps() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    let changes = vec![MemoryChange {
        key: "a-single-memory-with-a-long-key-for-a-narrow-notice".into(),
        operation: MemoryOperation::Created,
        scope: MemoryChangeScope::Auto,
    }];
    app.handle_agent_message(SessionMessage::Event(ServerEvent::MemoryChanged {
        id: MemoryChangeId("change-wrap".into()),
        origin: MemoryChangeOrigin::PrimaryAgent,
        causality: MemoryChangeCausality::ThisTurn,
        changes,
    }));
    // Collapsed by default: the summary shows, the key stays behind the fold.
    let out = render_text(&app, 24, 40);
    assert!(
        out.contains("saved by the"),
        "a single change takes the producer label: {out}"
    );
    assert!(
        !out.contains("for-a-narrow-notice"),
        "the collapsed notice hides its key until expanded: {out}"
    );
    // Expanded (Ctrl+O on the summary), a long key wraps rather than clips.
    app.expanded_fold_groups.insert("mg#0".into());
    let out = render_text(&app, 24, 40);
    assert!(
        out.contains("for-a-narrow-notice"),
        "a single-change key that outgrows the row wraps instead of clipping: {out}"
    );
}

/// Regression: a memory-change notice must toggle open by the fold-click path
/// (a row whose tag routes click to toggle_fold_at_row), not fall through to
/// the subagent branch. Before the fix the notice carried a fold key but the
/// system tag, so a click went nowhere even though the row advertised
/// ctrl+o.
#[test]
fn test_notice_click_toggles_fold() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.handle_agent_message(SessionMessage::Event(ServerEvent::MemoryChanged {
        id: MemoryChangeId("click-1".into()),
        origin: MemoryChangeOrigin::PrimaryAgent,
        causality: MemoryChangeCausality::ThisTurn,
        changes: vec![MemoryChange {
            key: "alpha".into(),
            operation: MemoryOperation::Created,
            scope: MemoryChangeScope::Auto,
        }],
    }));
    // Render to publish last_row_fold_keys, what a click resolves against.
    let _rendered = render_text(&app, 100, 24);
    let ri = app
        .last_row_fold_keys
        .borrow()
        .iter()
        .position(|k| k.as_deref() == Some("mg#0"))
        .expect("the notice row carries its fold key");
    assert!(
        !app.expanded_fold_groups.contains("mg#0"),
        "the notice starts collapsed"
    );
    app.toggle_fold_at_row(ri);
    assert!(
        app.expanded_fold_groups.contains("mg#0"),
        "a click routes the notice to the fold toggle"
    );
    let out = render_text(&app, 100, 24);
    assert!(
        out.contains("⎿  created alpha"),
        "the opened notice shows its key: {out}"
    );
}

/// Regression: a reasoning-bearing thought keeps its ctrl+o affordance and
/// still toggles by the thought-click path.
#[test]
fn test_thought_keep_affordance() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.transcript.push(TranscriptLine::ThoughtFor {
        ms: Some(42_000),
        reasoning: Some("a train of thought that expands inline".into()),
        tool_summary: None,
        turn_id: "t1".into(),
    });
    let out = render_text(&app, 100, 24);
    assert!(
        out.contains("Thought for 42s") && out.contains("(ctrl+o to expand)"),
        "a reasoning thought advertises its expand affordance: {out}"
    );
    let _rendered = render_text(&app, 100, 24);
    let ri = app
        .last_row_turn_ids
        .borrow()
        .iter()
        .position(|t| t.as_deref() == Some("t1"))
        .expect("the thought row publishes its turn id");
    app.toggle_thinking_expand_at_row(ri);
    assert!(
        app.expanded_thinking.contains("t1"),
        "the thought still toggles open on the thought-click path"
    );
}

/// A notice advertises the toggle that matches its state: the collapsed
/// summary offers expand, and opening it offers collapse. An opened notice
/// carried no hint at all before, so the way back was undiscoverable.
#[test]
fn test_notice_hint_matches_state() {
    let mut app = app_with_notice(
        "hint-1",
        MemoryChangeCausality::ThisTurn,
        &[("alpha", MemoryOperation::Created)],
    );
    let out = render_text(&app, 100, 24);
    assert!(
        out.contains("(ctrl+o to expand)"),
        "the collapsed summary offers the expand toggle: {out}"
    );
    app.expanded_fold_groups.insert("mg#0".into());
    let out = render_text(&app, 100, 24);
    assert!(
        out.contains("⎿  created alpha"),
        "the opened notice shows its key: {out}"
    );
    assert!(
        out.contains("(ctrl+o to collapse)"),
        "the opened summary offers the collapse toggle: {out}"
    );
    assert!(
        !out.contains("(ctrl+o to expand)"),
        "an open notice does not still offer to expand: {out}"
    );
}

/// Every row a notice publishes fits one terminal line: the draw pass slices
/// its row list by the fold-aware count, so a summary wider than the pane has
/// to wrap into rows of its own rather than draw two lines for one row.
#[test]
fn test_notice_rows_fit_pane() {
    let app = app_with_notice(
        "narrow-1",
        MemoryChangeCausality::ThisTurn,
        &[("alpha", MemoryOperation::Created)],
    );
    let _out = render_text(&app, 24, 40);
    let rows = app.last_all_rows.borrow();
    assert!(!rows.is_empty(), "the notice publishes rows");
    for (_, row) in rows.iter() {
        assert_eq!(
            wrap_line(row, 24).len(),
            1,
            "row outgrows the pane and would draw as several lines: {row:?}"
        );
    }
}

/// The fold-aware count follows the wrap the draw pass performs: at a width
/// where the summary wraps, a collapsed notice counts the rows it drew, and
/// the count matches the rows the draw published. It was a constant one
/// whatever the width before, so a scroll offset built from it drifted.
#[test]
fn test_notice_count_matches_render() {
    let mut app = app_with_notice(
        "count-1",
        MemoryChangeCausality::ThisTurn,
        &[
            ("alpha", MemoryOperation::Created),
            ("beta", MemoryOperation::Deleted),
        ],
    );
    let _out = render_text(&app, 24, 40);
    let collapsed = app.fold_aware_rows(None);
    assert!(
        collapsed > 1,
        "the summary outgrows a 24-column pane, so it counts its wrapped rows: {collapsed}"
    );
    assert_eq!(
        collapsed,
        app.transcript_scroll.total.get(),
        "the count and the rows the draw published agree"
    );
    app.expanded_fold_groups.insert("mg#0".into());
    let _out = render_text(&app, 24, 40);
    let expanded = app.fold_aware_rows(None);
    assert!(
        expanded > collapsed,
        "opening the notice adds its key rows: {collapsed} -> {expanded}"
    );
    assert_eq!(
        expanded,
        app.transcript_scroll.total.get(),
        "the count and the rows the draw published agree while open too"
    );
}

/// The cursor walk counts a notice's wrapped rows the way the draw pass drew
/// them, so a cursor on the delegation below the notice resolves to that
/// delegation. The walk and the draw are separate traversals of the same slot
/// list, so a change to one of them alone shows up here.
#[test]
fn test_notice_walk_matches_render() {
    let mut app = app_with_notice(
        "cursor-1",
        MemoryChangeCausality::ThisTurn,
        &[("alpha", MemoryOperation::Created)],
    );
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    let _out = render_text(&app, 24, 40);
    let head_row = app
        .last_all_rows
        .borrow()
        .iter()
        .position(|(_, row)| row.contains("found auth"))
        .expect("the delegation head rendered");
    app.selection.anchor = Some((0, head_row));
    assert!(
        app.toggle_subagent_expand(),
        "the cursor on the delegation below the notice resolves to it (row {head_row})"
    );
}

/// Regression: Ctrl+O with no cursor still opens and closes the newest
/// memory-change notice (the no-cursor fallback), so a notice is usable the
/// way a tool group is from the keyboard without a mouse anchor.
#[test]
fn test_notice_ctrl_o_latest() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.handle_agent_message(SessionMessage::Event(ServerEvent::MemoryChanged {
        id: MemoryChangeId("k1".into()),
        origin: MemoryChangeOrigin::PrimaryAgent,
        causality: MemoryChangeCausality::ThisTurn,
        changes: vec![MemoryChange {
            key: "alpha".into(),
            operation: MemoryOperation::Created,
            scope: MemoryChangeScope::Auto,
        }],
    }));
    assert!(app.selection.anchor.is_none(), "no cursor");
    handle_ctrl_o(&mut app);
    assert!(
        app.expanded_fold_groups.contains("mg#0"),
        "no-cursor Ctrl+O opens the notice"
    );
    handle_ctrl_o(&mut app);
    assert!(
        !app.expanded_fold_groups.contains("mg#0"),
        "the same key closes it again"
    );
}
