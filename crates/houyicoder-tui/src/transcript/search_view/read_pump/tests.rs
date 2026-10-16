//! What the search view's reads promise to the reader: which rows are on
//! screen while the log read runs, what a landing replaces them with, and which
//! landings a view that moved on refuses.

use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::KeyCode;

use super::*;
use crate::agent_message::SessionMessage;
use crate::records::TranscriptLine;
use crate::session::SessionConnection;
use crate::test_harness::{MockSnapshot, render_text, working_app};
use crate::transcript::rows::NamedRows;
use crate::transcript::scroll::SEARCH_LOG_THRESHOLD;
use crate::transcript::snapshot::{
    ReadError, ScanProgress, SnapshotLoad, TranscriptSnapshot, WindowLoad,
};

/// One window holding the given rows, spanning the given bytes.
fn window(rows: Vec<TranscriptLine>, start_offset: u64, next_offset: u64) -> WindowLoad {
    WindowLoad {
        rows: NamedRows::from_lines(rows),
        start_offset,
        next_offset,
        skipped: 0,
        bytes_total: next_offset,
    }
}

/// A working App whose session holds a connection-backed receiver, so a read
/// dispatches to a worker the test then pumps rather than reading inline.
fn connected_app() -> App {
    let mut app = working_app();
    let (_tx, rx) = channel::<SessionMessage>();
    app.sessions.current.connection = Some(SessionConnection::from_receiver(rx));
    app
}

/// Give the App live rows and a log over the size threshold holding its own,
/// unrelated rows: which rows are on screen says which of the two the view is
/// showing.
fn app_over_log(log_bytes: u64, window_rows: Vec<TranscriptLine>) -> App {
    let mut app = connected_app();
    app.sessions.current.transcript = vec![TranscriptLine::User("a live row".into())].into();
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: vec![TranscriptLine::Agent("only the log holds this".into())],
        log_bytes,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows: if window_rows.is_empty() {
            Vec::new()
        } else {
            vec![window(window_rows, 1024, log_bytes)]
        },
        scan_steps: 0,
        scan_calls: AtomicU32::new(0),
    }));
    app
}

/// Pump the search read until it lands or the deadline passes. Mirrors what the
/// app loop does each pass, but blocks the test thread so the result is applied
/// before the assertions run. Returns false when no read was on file at all.
fn pump_search_read_blocking(app: &mut App, timeout: Duration) -> bool {
    if !app.sessions.current.search_reads.holds_pending() {
        return false;
    }
    let start = Instant::now();
    while start.elapsed() < timeout {
        if app.pump_search_read() {
            return true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    false
}

/// The view opens over the rows on hand while the log read is still running:
/// the reader sees a searchable transcript in the frame the query was typed,
/// rather than a frame held until the log answers.
#[test]
fn test_enter_shows_live_rows() {
    let mut app = app_over_log(SEARCH_LOG_THRESHOLD + 1, vec![]);

    app.enter_search_view("live");

    assert!(
        app.ui.transcript_view.search_active(),
        "the search view is open"
    );
    assert!(
        app.sessions.current.search_reads.holds_pending(),
        "the log read was dispatched rather than run on the key path"
    );
    let out = render_text(&app, 80, 30);
    assert!(
        out.contains("a live row"),
        "the rows on hand render while the log read runs:\n{out}"
    );
}

/// The landed read replaces those rows with the log's own and focuses the
/// newest match there, so the reader's search runs against the whole log rather
/// than against whatever the resident window held.
#[test]
fn test_landing_replaces_corpus() {
    let mut app = app_over_log(
        SEARCH_LOG_THRESHOLD + 1,
        vec![
            TranscriptLine::Agent("an older tail row".into()),
            TranscriptLine::User("a tail needle".into()),
        ],
    );
    app.enter_search_view("needle");

    assert!(
        pump_search_read_blocking(&mut app, Duration::from_secs(5)),
        "the tail window read landed"
    );

    let out = render_text(&app, 80, 30);
    assert!(
        out.contains("a tail needle"),
        "the log's own rows render after the landing:\n{out}"
    );
    assert!(
        !out.contains("a live row"),
        "the rows the view opened over are gone:\n{out}"
    );
    assert_eq!(
        app.ui.transcript_view.search_focused_line(),
        Some(1),
        "the newest match of the landed corpus is the focused one"
    );
    assert!(
        app.window_browsing(),
        "a log over the threshold is browsed a window at a time"
    );
}

/// A step landing moves the window to the one walked to and focuses the match
/// nearest the direction walked, so pressing the step key again continues from
/// there.
#[test]
fn test_step_landing_moves_window() {
    let mut app = app_over_log(
        SEARCH_LOG_THRESHOLD + 1,
        vec![TranscriptLine::User("a tail needle".into())],
    );
    app.enter_search_view("needle");
    assert!(
        pump_search_read_blocking(&mut app, Duration::from_secs(5)),
        "the tail window read landed"
    );
    // The older window holds two matches, so the walk can land on either
    // depending on the direction, and the focus says which.
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: SEARCH_LOG_THRESHOLD + 1,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows: vec![window(
            vec![
                TranscriptLine::User("an older needle".into()),
                TranscriptLine::User("a newer needle".into()),
            ],
            0,
            1024,
        )],
        scan_steps: 0,
        scan_calls: AtomicU32::new(0),
    }));

    app.window_search_older();

    assert!(
        pump_search_read_blocking(&mut app, Duration::from_secs(5)),
        "the older window read landed"
    );
    let out = render_text(&app, 80, 30);
    assert!(
        out.contains("an older needle"),
        "the window walked to is the one on screen:\n{out}"
    );
    assert_eq!(
        app.ui.transcript_view.search_focused_line(),
        Some(1),
        "walking older focuses the newest match of the window it landed on"
    );
}

/// A query changed while a step was walking refuses that landing: the window
/// was chosen for a query the reader is no longer searching, so replacing the
/// corpus with it would move them somewhere they did not ask to go.
#[test]
fn test_changed_query_drops_step() {
    let mut app = app_over_log(
        SEARCH_LOG_THRESHOLD + 1,
        vec![TranscriptLine::User("a tail needle".into())],
    );
    app.enter_search_view("needle");
    assert!(
        pump_search_read_blocking(&mut app, Duration::from_secs(5)),
        "the tail window read landed"
    );
    let anchor_before = app.window_anchor();
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: SEARCH_LOG_THRESHOLD + 1,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows: vec![window(
            vec![TranscriptLine::User("an older needle".into())],
            0,
            1024,
        )],
        scan_steps: 0,
        scan_calls: AtomicU32::new(0),
    }));
    app.window_search_older();
    // The slash bar's commit is the way a query changes under an open view: it
    // re-runs the search over the corpus without asking the log for anything.
    app.enter_search_input();
    app.ui
        .transcript_view
        .handle_search_input_key(KeyCode::Char('x'));
    app.commit_search_input();

    assert!(
        pump_search_read_blocking(&mut app, Duration::from_secs(5)),
        "the window the walk reached came back and was refused"
    );

    assert_eq!(
        app.window_anchor(),
        anchor_before,
        "the landing was refused, so the window on screen did not move"
    );
}

/// Leaving the view while a read runs refuses its landing: the rows it brings
/// belong to a search the reader has left.
#[test]
fn test_exit_drops_landing() {
    let mut app = app_over_log(SEARCH_LOG_THRESHOLD + 1, vec![]);
    app.enter_search_view("nothing matches this");
    assert!(
        app.sessions.current.search_reads.holds_pending(),
        "the log read is running when the reader leaves"
    );

    app.exit_search_view();

    assert!(
        !app.sessions.current.search_reads.holds_pending(),
        "the read went with the view rather than waiting to land in it"
    );
    assert!(
        !app.ui.transcript_view.search_active(),
        "the search view is closed"
    );
}

/// A stage fold closes the view where it stands, without the App exit that
/// invalidates: the pump drops the read anyway, so a walk cannot keep reading
/// the log for a reader who is no longer searching.
#[test]
fn test_closed_view_stops_walk() {
    let mut app = app_over_log(SEARCH_LOG_THRESHOLD + 1, vec![]);
    app.enter_search_view("nothing matches this");
    let epoch = app.sessions.current.search_reads.epoch();
    let view = &mut app.ui.transcript_view;
    view.exit_search_if_active();

    app.pump_search_read();

    assert_ne!(
        app.sessions.current.search_reads.epoch(),
        epoch,
        "the closed view's read was dropped and the generation moved on"
    );
    assert!(
        !app.sessions.current.search_reads.holds_pending(),
        "nothing is left on file for a view that is gone"
    );
}

/// A log that cannot be read at all leaves the reader with the rows on hand and
/// says so: the search stays usable, and the failure is not silent.
#[test]
fn test_unreadable_log_keeps_rows() {
    struct UnreadableLog;
    impl TranscriptSnapshot for UnreadableLog {
        fn log_size(&self) -> u64 {
            SEARCH_LOG_THRESHOLD + 1
        }
        fn load(&self, _max_bytes: u64) -> SnapshotLoad {
            SnapshotLoad::default()
        }
        fn window(&self, _anchor: u64, _max_bytes: u64) -> WindowLoad {
            WindowLoad::default()
        }
        fn tail_window(&self, _max_bytes: u64) -> Result<WindowLoad, ReadError> {
            Err(ReadError::Io)
        }
        fn window_before(&self, _from_byte: u64, _max_bytes: u64) -> Result<WindowLoad, ReadError> {
            Err(ReadError::Io)
        }
        fn scan_chunk(&self) -> ScanProgress {
            ScanProgress::default()
        }
        fn byte_at(&self, _event_idx: usize) -> Option<u64> {
            None
        }
        fn event_count(&self) -> Option<usize> {
            None
        }
    }
    let mut app = connected_app();
    app.sessions.current.transcript = vec![TranscriptLine::User("a live row".into())].into();
    app.sessions.persistence.snapshot = Some(Arc::new(UnreadableLog));

    app.enter_search_view("live");
    assert!(
        app.sessions.current.search_reads.holds_pending(),
        "the read runs on a worker"
    );
    assert!(
        pump_search_read_blocking(&mut app, Duration::from_secs(5)),
        "the failed read came back"
    );

    let out = render_text(&app, 80, 30);
    assert!(
        out.contains("a live row"),
        "the rows on hand stay on screen:\n{out}"
    );
    assert!(
        out.contains("session log unreadable"),
        "the failure is on screen rather than swallowed:\n{out}"
    );
}
