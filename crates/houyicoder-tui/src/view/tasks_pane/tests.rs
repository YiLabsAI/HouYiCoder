//! The /tasks pane: what the list and the detail render, and the keys that
//! open, drill into, stop, and close them.

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::envelope::ClientFrame;
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::tasks::{
    BackgroundTaskDetail, BackgroundTaskId, BackgroundTaskSnapshot, BackgroundTaskStatus,
};

use crate::composition;
use crate::keys::handle_working;
use crate::state::{App, Pane, Screen};
use crate::test_harness::{
    TransportEvent, connected_app_with_events, render_text, wait_for_request,
};

use super::{body_lines, header_line, task_lines};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn snapshot(
    id: u64,
    status: BackgroundTaskStatus,
    command: &str,
    elapsed_ms: u64,
    exit_code: Option<i32>,
) -> BackgroundTaskSnapshot {
    BackgroundTaskSnapshot {
        id: BackgroundTaskId(id),
        status,
        command: command.into(),
        elapsed_ms,
        exit_code,
    }
}

/// The pane open on a list of one running and one finished task.
fn tasks_app() -> App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Tasks;
    app.sessions.current.capabilities.tasks.replace_all(vec![
        snapshot(1, BackgroundTaskStatus::Running, "cargo build", 4_000, None),
        snapshot(
            2,
            BackgroundTaskStatus::Finished,
            "npm test",
            72_400,
            Some(0),
        ),
    ]);
    app
}

fn detail(id: u64, status: BackgroundTaskStatus) -> BackgroundTaskDetail {
    BackgroundTaskDetail {
        id: BackgroundTaskId(id),
        status,
        command: "cargo build".into(),
        elapsed_ms: 5_000,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        tail: Vec::new(),
    }
}

/// Open one task's detail and fill it with a record, as a reply would.
fn open_detail(app: &mut App, record: BackgroundTaskDetail) {
    app.ui
        .tasks_pane
        .open_detail(record.id, RequestId(1), Instant::now());
    assert!(app.ui.tasks_pane.apply_detail(RequestId(1), record));
}

/// Every task gets a row carrying its number, state, run time, and command,
/// and the count above the list says how many of them still run.
#[test]
fn test_list_rows_render() {
    let app = tasks_app();
    let out = render_text(&app, 80, 24);
    assert!(out.contains("2 tasks \u{00b7} 1 running"), "{out}");
    assert!(out.contains("#1") && out.contains("#2"), "{out}");
    assert!(out.contains("running") && out.contains("finished"), "{out}");
    assert!(out.contains("4s"), "{out}");
    assert!(out.contains("1m 12s"), "{out}");
    assert!(
        out.contains("cargo build") && out.contains("npm test"),
        "each row names its command: {out}"
    );
    assert!(
        out.contains("Enter to open") && out.contains("Esc to close"),
        "{out}"
    );
}

/// The cursor marker sits on the selected row and moves with Down.
#[test]
fn test_list_marker_follows_cursor() {
    let mut app = tasks_app();
    let first = render_text(&app, 80, 24);
    assert!(first.contains("\u{25b6} #1"), "{first}");
    assert!(!first.contains("\u{25b6} #2"), "{first}");
    handle_working(&mut app, key(KeyCode::Down));
    let second = render_text(&app, 80, 24);
    assert!(second.contains("\u{25b6} #2"), "{second}");
    assert!(!second.contains("\u{25b6} #1"), "{second}");
}

/// A session that moved nothing to the background says so rather than
/// showing a blank pane with no explanation.
#[test]
fn test_empty_list_explains() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Tasks;
    let out = render_text(&app, 80, 24);
    assert!(out.contains("0 tasks"), "{out}");
    assert!(out.contains("No background commands."), "{out}");
    assert!(
        !out.contains("k to kill") && !out.contains("Enter to open"),
        "an empty list offers neither action: {out}"
    );
}

/// The kill hint belongs to the selected row: moving onto an ended task drops
/// it, because that row has nothing left to stop.
#[test]
fn test_kill_hint_tracks_selection() {
    let mut app = tasks_app();
    let first = render_text(&app, 80, 24);
    assert!(first.contains("k to kill"), "{first}");
    handle_working(&mut app, key(KeyCode::Down));
    let second = render_text(&app, 80, 24);
    assert!(!second.contains("k to kill"), "{second}");
    assert!(second.contains("Enter to open"), "{second}");
}

/// A run measured below a second states its milliseconds rather than rounding
/// to a zero it never took.
#[test]
fn test_row_reports_milliseconds() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Tasks;
    app.sessions
        .current
        .capabilities
        .tasks
        .replace_all(vec![snapshot(
            1,
            BackgroundTaskStatus::Finished,
            "true",
            420,
            Some(0),
        )]);
    let out = render_text(&app, 80, 24);
    assert!(out.contains("420ms"), "{out}");
    let mut record = detail(1, BackgroundTaskStatus::Finished);
    record.elapsed_ms = 420;
    open_detail(&mut app, record);
    let out = render_text(&app, 80, 24);
    assert!(out.contains("420ms"), "{out}");
    app.ui.tasks_pane.close_detail();
    app.sessions
        .current
        .capabilities
        .tasks
        .replace_all(vec![snapshot(
            1,
            BackgroundTaskStatus::Killed,
            "true",
            0,
            None,
        )]);
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("<1ms"),
        "a zero span reads as under a millisecond: {out}"
    );
}

/// An ended task's detail shows the output the command produced, with
/// standard error under its own label.
#[test]
fn test_detail_shows_output() {
    let mut app = tasks_app();
    let mut record = detail(2, BackgroundTaskStatus::Finished);
    record.stdout = "Compiling houyicoder-tui\nFinished dev profile".into();
    record.stderr = "warning: unused variable".into();
    open_detail(&mut app, record);
    let out = render_text(&app, 80, 24);
    assert!(out.contains("#2 cargo build"), "{out}");
    assert!(out.contains("finished \u{00b7} 5s"), "{out}");
    assert!(out.contains("Compiling houyicoder-tui"), "{out}");
    assert!(out.contains("Finished dev profile"), "{out}");
    assert!(out.contains("stderr"), "{out}");
    assert!(out.contains("warning: unused variable"), "{out}");
    assert!(
        !out.contains("k to kill"),
        "an ended task shows no kill hint: {out}"
    );
    assert!(out.contains("Esc to back"), "{out}");
}

/// An ended task shows how it ended, its run time, and its exit status, and
/// drops the kill hint because there is nothing left to stop.
#[test]
fn test_detail_reports_ending() {
    let mut app = tasks_app();
    let mut record = detail(2, BackgroundTaskStatus::Finished);
    record.elapsed_ms = 72_400;
    record.exit_code = Some(0);
    record.stdout = "all tests passed".into();
    open_detail(&mut app, record);
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("finished \u{00b7} 1m 12s \u{00b7} exit 0"),
        "{out}"
    );
    assert!(out.contains("all tests passed"), "{out}");
    assert!(!out.contains("k to kill"), "nothing left to stop: {out}");
}

/// A task that has not printed through the captured streams yet shows the
/// live tail instead of an empty body.
#[test]
fn test_detail_shows_tail() {
    let mut app = tasks_app();
    let mut record = detail(1, BackgroundTaskStatus::Running);
    record.tail = vec!["building 12 of 40".into()];
    open_detail(&mut app, record);
    let out = render_text(&app, 80, 24);
    assert!(out.contains("building 12 of 40"), "{out}");
}

/// Terminal control sequences a command prints are markup for a terminal,
/// not cells: a title-set escape in the command, in the output, or in the
/// live tail must reach the pane as the text around it, never as the bytes
/// themselves. The pane's own rows are read here because the cell buffer
/// already drops control characters, which would hide a row that never
/// stripped them.
#[test]
fn test_pane_strips_escape_codes() {
    let task = snapshot(
        1,
        BackgroundTaskStatus::Running,
        "\u{1b}]0;owned\u{7}cargo build",
        4_000,
        None,
    );
    let (rows, _) = task_lines(&[&task], 0, 80);
    let list: String = rows[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(
        !list.contains('\u{1b}'),
        "the row shows no escape: {list:?}"
    );
    assert!(list.contains("cargo build"), "{list}");

    let mut record = detail(1, BackgroundTaskStatus::Running);
    record.command = "\u{1b}]0;owned\u{7}cargo build".into();
    record.stdout = "\u{1b}[31mwarning: red\u{1b}[0m".into();
    record.stderr = "\u{1b}[33merror: yellow\u{1b}[0m".into();
    record.tail = vec!["\u{1b}[36mtail text\u{1b}[0m".into()];
    let head: String = header_line(BackgroundTaskId(1), Some(&record), 80)
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(!head.contains('\u{1b}'), "the header shows no escape");
    let body: String = body_lines(Some(&record))
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !body.contains('\u{1b}'),
        "the output shows no escape: {body:?}"
    );
    assert!(body.contains("warning: red"), "{body}");
    assert!(body.contains("error: yellow"), "{body}");

    let mut held = detail(1, BackgroundTaskStatus::Running);
    held.tail = record.tail.clone();
    let tail: String = body_lines(Some(&held))
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !tail.contains('\u{1b}'),
        "the tail shows no escape: {tail:?}"
    );
    assert!(tail.contains("tail text"), "{tail}");
}

/// Scrolling the detail moves the visible window: the first lines leave the
/// screen as later output arrives, and the state row above stays put.
#[test]
fn test_detail_scrolls_output() {
    let mut app = tasks_app();
    let mut record = detail(2, BackgroundTaskStatus::Finished);
    record.stdout = (1..=60)
        .map(|n| format!("line {n:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    open_detail(&mut app, record);
    let first = render_text(&app, 80, 24);
    assert!(first.contains("line 01"), "{first}");
    assert!(!first.contains("line 60"), "{first}");
    for _ in 0..6 {
        handle_working(&mut app, key(KeyCode::PageDown));
    }
    let later = render_text(&app, 80, 24);
    assert!(later.contains("line 60"), "{later}");
    assert!(!later.contains("line 01"), "{later}");
    assert!(
        later.contains("finished \u{00b7} 5s"),
        "the state row stays fixed: {later}"
    );
}

/// Down on an empty input opens the pane and asks the host for the list.
#[test]
fn test_down_opens_tasks_pane() {
    let (mut app, events) = connected_app_with_events();
    app.sessions
        .current
        .capabilities
        .tasks
        .replace_all(vec![snapshot(
            1,
            BackgroundTaskStatus::Running,
            "cargo build",
            4_000,
            None,
        )]);
    app.ui.input.editor.clear();
    app.ui.navigation.pane = Pane::Transcript;
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(app.ui.navigation.pane, Pane::Tasks);
    wait_for_request(&events, |req| {
        matches!(req, FrontendRequest::ListBackgroundTasks)
    });
}

/// Esc in the detail returns to the list, and Esc there closes the pane: one
/// level per press, never both.
#[test]
fn test_esc_returns_to_list() {
    let mut app = tasks_app();
    open_detail(&mut app, detail(2, BackgroundTaskStatus::Finished));
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(app.ui.navigation.pane, Pane::Tasks, "the list stays open");
    assert!(app.ui.tasks_pane.detail().is_none(), "the detail closed");
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(app.ui.navigation.pane, Pane::Transcript);
}

/// Enter on a row opens that task's detail and asks the host for its record.
#[test]
fn test_enter_opens_task_detail() {
    let (mut app, events) = connected_app_with_events();
    app.sessions
        .current
        .capabilities
        .tasks
        .replace_all(vec![snapshot(
            1,
            BackgroundTaskStatus::Running,
            "cargo build",
            4_000,
            None,
        )]);
    app.ui.navigation.pane = Pane::Tasks;
    handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.ui.tasks_pane.detail().map(|detail| detail.id()),
        Some(BackgroundTaskId(1))
    );
    wait_for_request(&events, |req| {
        matches!(
            req,
            FrontendRequest::BackgroundTaskDetail { id } if *id == BackgroundTaskId(1)
        )
    });
}

/// k stops the running task under the cursor by asking the host to kill it.
#[test]
fn test_kill_stops_running_task() {
    let (mut app, events) = connected_app_with_events();
    app.sessions
        .current
        .capabilities
        .tasks
        .replace_all(vec![snapshot(
            1,
            BackgroundTaskStatus::Running,
            "cargo build",
            4_000,
            None,
        )]);
    app.ui.navigation.pane = Pane::Tasks;
    handle_working(&mut app, key(KeyCode::Char('k')));
    wait_for_request(&events, |req| {
        matches!(
            req,
            FrontendRequest::KillBackgroundTask { id } if *id == BackgroundTaskId(1)
        )
    });
}

/// A finished task has nothing to stop: k sends no kill and tells the user
/// why it did nothing.
#[test]
fn test_kill_skips_finished_task() {
    let (mut app, events) = connected_app_with_events();
    app.sessions
        .current
        .capabilities
        .tasks
        .replace_all(vec![snapshot(
            1,
            BackgroundTaskStatus::Finished,
            "npm test",
            9_000,
            Some(0),
        )]);
    app.ui.navigation.pane = Pane::Tasks;
    handle_working(&mut app, key(KeyCode::Char('k')));
    assert!(
        !kill_request_sent(&events),
        "an ended task must not be killed"
    );
    let out = render_text(&app, 80, 24);
    assert!(out.contains("#1: not running"), "{out}");
}

/// Whether a kill request reached the transport within the wait window.
fn kill_request_sent(events: &Receiver<TransportEvent>) -> bool {
    let mut sent = false;
    while let Ok(TransportEvent::Frame(frame)) = events.recv_timeout(Duration::from_millis(100)) {
        if let Ok(ClientFrame::Request(request)) = serde_json::from_str(&frame)
            && matches!(request.payload, FrontendRequest::KillBackgroundTask { .. })
        {
            sent = true;
        }
    }
    sent
}
