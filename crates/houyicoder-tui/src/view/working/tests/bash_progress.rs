//! Running-row render, row-count agreement, and progress dispatch.

use std::iter::once;

use houyicoder_protocol::frontend::session_update::{
    SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use houyicoder_protocol::frontend::tasks::{
    BackgroundTaskId, BackgroundTaskSnapshot, BackgroundTaskStatus,
};

use crate::agent_message::{ServerEvent, SessionMessage};
use crate::composition;
use crate::records::{ToolOutcome, TranscriptLine};
use crate::rows::row_set::RowSet;
use crate::state::{App, BashProgress, Screen};
use crate::test_harness::render_text;
use crate::transcript::frame::TranscriptFrame;
use crate::view::working::transcript::push_line_rows;

fn tcall(cid: &str, name: &str, brief: &str, oc: ToolOutcome) -> TranscriptLine {
    TranscriptLine::Tool {
        name: name.to_string(),
        tool: name.to_string(),
        status: brief.to_string(),
        invocation: brief.to_string(),
        outcome: oc,
        call_id: cid.to_string(),
        background_task: None,
        body: String::new(),
        is_diff: false,
    }
}

fn bash_result(cid: &str, body: &str, task: Option<u64>) -> TranscriptLine {
    TranscriptLine::Tool {
        name: "result".to_string(),
        tool: "bash".to_string(),
        status: String::new(),
        invocation: String::new(),
        outcome: ToolOutcome::Running,
        call_id: cid.to_string(),
        background_task: task.map(BackgroundTaskId),
        body: body.to_string(),
        is_diff: false,
    }
}

/// A running bash call with an active run and one ticked call, ready to
/// render. The run stays busy so the call keeps its rows instead of folding
/// into a summary.
fn app_with_tick(command: &str, progress: BashProgress) -> App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![
        TranscriptLine::User("hi".into()),
        tcall("c1", "bash", command, ToolOutcome::Success),
    ]
    .into();
    app.start_run_for_test(0);
    let tools = &mut app.run_progress_mut().expect("active run").tools;
    tools.begin("c1".into());
    tools.note_tick("c1".into(), progress);
    app
}

/// A call younger than the reporting threshold shows nothing under its chip;
/// from two seconds on it reports its elapsed time and the call's own limit,
/// so a long command reads as working rather than stuck.
#[test]
fn test_running_row_threshold() {
    let app = app_with_tick(
        "npm install",
        BashProgress {
            elapsed_secs: 1,
            timeout_secs: Some(600),
            ..Default::default()
        },
    );
    let out = render_text(&app, 80, 24);
    assert!(out.contains("Bash(npm install)"), "chip present: {out}");
    assert!(
        !out.contains("Running\u{2026}"),
        "under two seconds shows no row: {out}"
    );

    let app = app_with_tick(
        "npm install",
        BashProgress {
            elapsed_secs: 12,
            timeout_secs: Some(600),
            ..Default::default()
        },
    );
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Running\u{2026} (12s \u{00b7} timeout 10m)"),
        "elapsed time and the call's limit land under the chip: {out}"
    );
}

/// A call with no declared limit still reports its elapsed time, without an
/// empty limit segment.
#[test]
fn test_running_row_without_timeout() {
    let app = app_with_tick(
        "make check",
        BashProgress {
            elapsed_secs: 30,
            timeout_secs: None,
            ..Default::default()
        },
    );
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Running\u{2026} (30s)"),
        "the row reports seconds alone: {out}"
    );
    assert!(!out.contains("timeout"), "no limit was declared: {out}");
}

/// The running row previews the newest five output lines with the escape
/// sequences a colouring program wrote stripped, so terminal bytes never
/// reach the pane.
#[test]
fn test_running_row_previews_tail() {
    let tail: Vec<String> = (1..=7)
        .map(|n| format!("step {n} of 7"))
        .chain(once("\u{1b}[32mstep 8 ok\u{1b}[0m".to_string()))
        .collect();
    let app = app_with_tick(
        "cargo build",
        BashProgress {
            elapsed_secs: 5,
            timeout_secs: Some(600),
            tail,
        },
    );
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("step 4 of 7"),
        "the last five lines show: {out}"
    );
    assert!(
        !out.contains("step 3 of 7"),
        "older lines stay out of the preview: {out}"
    );
    assert!(
        out.contains("step 8 ok"),
        "the newest line is previewed: {out}"
    );
    assert!(
        !out.contains("[32m"),
        "the preview carries no escape sequence: {out}"
    );
}

/// The row-count path and the draw path read one answer for how many rows a
/// running call takes. Two separate calculations would send the scroll offset
/// past rows the draw never emitted.
#[test]
fn test_running_rows_count() {
    let app = app_with_tick(
        "cargo build",
        BashProgress {
            elapsed_secs: 9,
            timeout_secs: Some(600),
            tail: vec!["Compiling a".into(), "Compiling b".into()],
        },
    );
    drop(render_text(&app, 60, 24));
    let width = app.ui.frame.feedback.transcript_width.get();
    let line = tcall("c1", "bash", "cargo build", ToolOutcome::Success);
    let counted = app.line_display_rows(&line);
    let mut row_set = RowSet::default();
    assert!(push_line_rows(&line, None, width, &app, &mut row_set));
    assert_eq!(
        row_set.len(),
        counted,
        "the count path and the draw path must agree: {:?}",
        row_set
            .rows()
            .iter()
            .map(|row| row.1.as_str())
            .collect::<Vec<_>>()
    );
}

/// A background command's row reports how its task ended once the host
/// publishes that state, keeping the row it already had.
#[test]
fn test_background_row_reports_ending() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![
        TranscriptLine::User("hi".into()),
        tcall("c1", "bash", "cargo build", ToolOutcome::Success),
        bash_result(
            "c1",
            "Running in background (task #3 \u{00b7} \u{2193} or /tasks to manage)",
            Some(3),
        ),
    ]
    .into();
    app.start_run_for_test(0);
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Running in background (task #3"),
        "the running form names the task and the key: {out}"
    );

    app.sessions
        .current
        .capabilities
        .tasks
        .record(BackgroundTaskSnapshot {
            id: BackgroundTaskId(3),
            status: BackgroundTaskStatus::Finished,
            command: "cargo build".into(),
            elapsed_ms: 72_400,
            exit_code: Some(0),
        });
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Finished in 1m 12s (exit 0)"),
        "the row reports the ending, its run time, and the exit status: {out}"
    );
    assert!(
        !out.contains("Running in background"),
        "the running form gives way to the ending: {out}"
    );
}

/// A row restored from the log whose task the host no longer holds reads as
/// released once the listing arrives, rather than claiming the command still
/// runs. Before the listing, a missing task means nothing was observed yet.
#[test]
fn test_released_task_reads_gone() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![
        TranscriptLine::User("hi".into()),
        tcall("c1", "bash", "cargo build", ToolOutcome::Success),
        bash_result(
            "c1",
            "Running in background (task #3 \u{00b7} \u{2193} or /tasks to manage)",
            Some(3),
        ),
    ]
    .into();
    app.start_run_for_test(0);
    let before = render_text(&app, 80, 24);
    assert!(
        before.contains("Running in background (task #3"),
        "no listing yet, so the row keeps its original text: {before}"
    );
    app.sessions
        .current
        .capabilities
        .tasks
        .replace_all(Vec::new());
    let out = render_text(&app, 80, 24);
    assert!(out.contains("task #3 is no longer tracked"), "{out}");
    assert!(!out.contains("Running in background"), "{out}");
}

// --- ToolProgress dispatch ---

fn tool_call_frame(id: &str, title: &str, status: ToolCallStatus) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(id, title).status(status),
    ))
}

fn tool_done_frame(id: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        id,
        ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
    )))
}

/// A ToolProgress tick lands in the ticker only while the call is in the
/// running set; the running row reads it, and the result clears it.
#[test]
fn test_tool_records_while_running() {
    let mut app = composition::app();
    app.start_run_for_test(0);
    // Call not in the running set yet: a stray tick (out of order) is
    // dropped, not stored.
    app.handle_agent_message(SessionMessage::Event(ServerEvent::ToolProgress {
        call_id: "call_1".into(),
        elapsed_secs: 3,
        timeout_secs: None,
        tail: Vec::new(),
    }));
    assert!(
        app.run_progress()
            .expect("active run")
            .tools
            .tick("call_1")
            .is_none(),
        "tick before the call is tracked is dropped"
    );
    // Running: the tick lands with its elapsed time, limit, and tail.
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        tool_call_frame("call_1", "bash", ToolCallStatus::InProgress).into(),
    )));
    app.handle_agent_message(SessionMessage::Event(ServerEvent::ToolProgress {
        call_id: "call_1".into(),
        elapsed_secs: 5,
        timeout_secs: Some(120),
        tail: vec!["14 of 50".into(), "15 of 50".into()],
    }));
    assert_eq!(
        app.run_progress().expect("active run").tools.tick("call_1"),
        Some(&BashProgress {
            elapsed_secs: 5,
            timeout_secs: Some(120),
            tail: vec!["14 of 50".into(), "15 of 50".into()],
        })
    );
    // Result lands: finishing the call clears the entry.
    app.handle_agent_message(SessionMessage::Event(ServerEvent::Frame(
        tool_done_frame("call_1").into(),
    )));
    assert!(
        app.run_progress()
            .expect("active run")
            .tools
            .tick("call_1")
            .is_none()
    );
}

/// The host's task snapshot replaces the one held for that number, and the
/// running count follows how many tasks have not ended.
#[test]
fn test_task_snapshot_replaces() {
    let mut app = composition::app();
    let tasks = &mut app.sessions.current.capabilities.tasks;
    tasks.record(BackgroundTaskSnapshot {
        id: BackgroundTaskId(1),
        status: BackgroundTaskStatus::Running,
        command: "cargo build".into(),
        elapsed_ms: 4_000,
        exit_code: None,
    });
    assert_eq!(tasks.running_count(), 1);
    tasks.record(BackgroundTaskSnapshot {
        id: BackgroundTaskId(1),
        status: BackgroundTaskStatus::Finished,
        command: "cargo build".into(),
        elapsed_ms: 9_000,
        exit_code: Some(0),
    });
    assert_eq!(
        tasks.iter().count(),
        1,
        "a snapshot replaces the one for its number"
    );
    assert_eq!(tasks.running_count(), 0, "an ended task is not running");
}
