//! The parked viewport holds its place by row identity.
//!
//! A reader who scrolled back stands on the row the pane drew, not on a row
//! number: the same text can appear many times above it, a narrower terminal
//! re-wraps the rows, a group above it folds open or shut, and rows read back
//! from the session log land ahead of it. Each test moves the rows under the
//! viewport and reads the pane the reader is left looking at, so a viewport
//! that held a number instead of a row draws different rows and reddens.

use ratatui::buffer::Buffer;
use serde_json::json;

use houyicoder_protocol::frontend::SlashCommand;
use houyicoder_protocol::frontend::session_update::{
    SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};

use crate::composition;
use crate::records::TranscriptLine;
use crate::state::{App, Screen};
use crate::test_harness::render_buffer;
use crate::transcript::TranscriptFrame;
use crate::transcript::rows::NamedRows;

/// The pane the tests read: wide enough that a marker takes one row, narrow
/// enough when narrowed that the long line re-wraps, and tall enough that a
/// park lands a viewport above the tail.
const WIDTH: u16 = 80;
const NARROW: u16 = 40;
const HEIGHT: u16 = 24;

/// How far above the tail a test parks. A park that lands on the last screen
/// clamps back to the tail, so the parked row must stand a viewport back for
/// the pane to read from it at all.
const PARK_BACK: usize = 30;

/// The copies of the repeated text, enough that the region they fill is many
/// screens long.
const COPIES: usize = 20;

fn working() -> App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app
}

fn tool_call(id: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(id, "bash")
            .raw_input(json!({"command": "ls -la"}))
            .status(ToolCallStatus::InProgress),
    ))
}

fn tool_result(id: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        id,
        ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(json!({"ok": true})),
    )))
}

/// A transcript with room to move under the reader: a foldable pair of tool
/// calls, a line long enough to re-wrap at a narrower width, then copies of
/// one text with a marker of their own after each. The rows come from frames,
/// so the rebuild a later line triggers keeps them.
fn corpus(app: &mut App) {
    app.system_line("opening turn");
    app.sessions.current.transcript.push_frame(tool_call("c1"));
    app.sessions
        .current
        .transcript
        .push_frame(tool_result("c1"));
    app.sessions.current.transcript.push_frame(tool_call("c2"));
    app.sessions
        .current
        .transcript
        .push_frame(tool_result("c2"));
    app.rebuild_transcript();
    app.system_line("L".repeat(240));
    for i in 0..COPIES {
        app.system_line("same text");
        app.system_line(format!("marker {i}"));
    }
}

/// The rows the transcript pane draws, read from a frame at the pane's own
/// rect. A viewport that held its place draws the same rows, so a scroll
/// assertion reads what the reader sees rather than the number under it.
fn pane_rows(app: &App, width: u16) -> Vec<String> {
    let buffer: Buffer = render_buffer(app, width, HEIGHT);
    let rect = app.ui.frame.layout.transcript_rect.get();
    let mut rows = Vec::new();
    for y in rect.y..rect.y + rect.height {
        let mut text = String::new();
        for x in rect.x..rect.x + rect.width {
            if let Some(cell) = buffer.cell((x, y)) {
                text.push_str(cell.symbol());
            }
        }
        rows.push(text.trim_end().to_string());
    }
    rows
}

/// The marker row drawn near the pane's top: the follower that tells one copy
/// of the repeated text from another.
fn marker_row(rows: &[String]) -> Option<String> {
    rows.iter()
        .take(4)
        .find(|row| row.contains("marker "))
        .map(|row| row.trim().to_string())
}

/// Park the viewport on a copy of the repeated text and report the marker that
/// follows it. Which copy it lands on is not fixed -- the corpus repeats -- so
/// the marker is what names it from then on.
fn park_on_repeated_copy(app: &mut App) -> String {
    // A frame precedes a key in a running terminal: the draw publishes both
    // the rows a park names its row from and the counts it clamps against.
    drop(pane_rows(app, WIDTH));
    let top = app.whole_scroll().top_offset();
    for back in PARK_BACK..PARK_BACK + 8 {
        app.park_scroll_at(top - back);
        let rows = pane_rows(app, WIDTH);
        if rows[0].contains("same text") {
            return marker_row(&rows).expect("the parked copy's marker follows it");
        }
    }
    panic!("the corpus draws no copy of the repeated text above the tail");
}

/// The pane still starts at the copy the reader stood on: the repeated text on
/// top and that copy's own marker just below it.
fn assert_pane_holds(rows: &[String], marker: &str) {
    assert!(
        rows[0].contains("same text"),
        "the pane starts at the parked row: {rows:?}"
    );
    assert_eq!(
        marker_row(rows).as_deref(),
        Some(marker),
        "the copy the reader stood on is still the one on top: {rows:?}"
    );
}

/// A group above the viewport opening adds rows over the reader's head. The
/// parked row is named, so the pane still starts there; a viewport holding a
/// row number would read from the rows the group pushed down instead, landing
/// on another copy of the same text.
#[test]
fn test_anchor_holds_repeated_row() {
    let mut app = working();
    corpus(&mut app);
    let marker = park_on_repeated_copy(&mut app);

    app.ui.transcript_view.toggle_fold("c1#0".to_string());

    assert_pane_holds(&pane_rows(&app, WIDTH), &marker);
}

/// A narrower terminal re-wraps the rows, so the parked row's position in the
/// list moves. The pane still starts at that row: resolution happens against
/// the rows the new width built, not against the position taken at the old
/// one.
#[test]
fn test_anchor_holds_across_resize() {
    let mut app = working();
    corpus(&mut app);
    let marker = park_on_repeated_copy(&mut app);

    assert_pane_holds(&pane_rows(&app, NARROW), &marker);
}

/// A group above the viewport closing takes rows out over the reader's head.
/// The parked row is named, so the pane still starts there rather than drifting
/// down into the rows that moved up under it.
#[test]
fn test_anchor_holds_across_fold() {
    let mut app = working();
    corpus(&mut app);
    app.ui.transcript_view.toggle_fold("c1#0".to_string());
    let marker = park_on_repeated_copy(&mut app);

    app.ui.transcript_view.toggle_fold("c1#0".to_string());

    assert_pane_holds(&pane_rows(&app, WIDTH), &marker);
}

/// Older rows read back from the session log land ahead of the viewport, and
/// the pass that installs them rebuilds the list under the reader. The parked
/// row is named, so the pane still starts there; a viewport holding a row
/// number would read from the rows that moved up under it instead, and here
/// those rows are long enough to wrap, so the mistake lands on another copy of
/// the repeated text rather than a row or two away.
#[test]
fn test_anchor_holds_across_prepend() {
    let mut app = working();
    corpus(&mut app);
    let marker = park_on_repeated_copy(&mut app);

    let older: Vec<TranscriptLine> = (0..3)
        .map(|i| TranscriptLine::User(format!("older {i} {}", "o".repeat(240))))
        .collect();
    app.transcript_mut()
        .prepend_disk_rows(NamedRows::from_lines(older), 100, 0);
    app.rebuild_transcript();

    assert_pane_holds(&pane_rows(&app, WIDTH), &marker);
}

/// A cleared session replaces the history under the parked view, so the row
/// the reader stood on is gone from the log rather than merely off screen.
/// The viewport returns to the tail and the reader is told: left holding the
/// old row number, the fresh session opens partway down its own transcript
/// with the newest lines off screen.
#[test]
fn test_clear_releases_parked_view() {
    let mut app = working();
    corpus(&mut app);
    park_on_repeated_copy(&mut app);

    app.run_command(SlashCommand::Clear);
    let wiped = pane_rows(&app, WIDTH);
    assert!(
        wiped
            .iter()
            .any(|row| row.contains("scroll position released")),
        "the reader is told the place went with the history: {wiped:?}"
    );

    for i in 0..30 {
        app.system_line(format!("fresh row {i}"));
    }
    let rows = pane_rows(&app, WIDTH);
    assert!(
        rows.iter().any(|row| row.contains("fresh row 29")),
        "the fresh session is followed from its tail: {rows:?}"
    );
}
