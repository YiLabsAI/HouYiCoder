//! Parent and child scroll isolation: the parent's scroll position survives
//! a visit into a teammate view, and the child's scrolling never runs the
//! parent's trim or new-message side effects.

use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::mpsc;

use crate::composition::app;
use crate::records::TranscriptLine;
use crate::state::App;
use crate::state::Screen;
use crate::state::history_read::{HistoryReadOutcome, HistoryReadResult, HistoryStamp};
use crate::test_harness::MockSnapshot;
use crate::test_harness::render_buffer;
use crate::test_harness::render_text;
use crate::transcript::TranscriptFrame;
use crate::transcript::rows::NamedRows;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};

fn user_frame(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}

fn subagent_line(rows: usize) -> TranscriptLine {
    TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "summary".into(),
        prompt: "task".into(),
        folded_transcript: (0..rows)
            .map(|i| TranscriptLine::Agent(format!("child row {i}")))
            .collect(),
        color: None,
    }
}

/// A working app whose parent transcript holds one user line per frame. The
/// Subagent line rides after the rebuild so the rebuild cannot drop it.
fn app_with_parent(parent_rows: usize) -> App {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    for i in 0..parent_rows {
        app.sessions
            .current
            .transcript
            .push_frame(user_frame(&format!("parent row {i}")));
    }
    app.rebuild_transcript();
    app.sessions.current.transcript.push(subagent_line(40));
    app
}

/// The rows the transcript pane draws, read from the frame at the pane's own
/// rect. A viewport that held its place draws the same rows, so a scroll
/// assertion reads what the reader sees rather than the offset under it.
fn pane_rows(app: &App) -> Vec<String> {
    let buffer = render_buffer(app, 80, 24);
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

/// Entering the child view starts the child scroll at the tail and leaves the
/// parent scroll where the reader parked it, so the exit renders the same
/// parent rows again.
#[test]
fn test_parent_scroll_survives_visit() {
    let mut app = app_with_parent(60);
    let first = render_text(&app, 80, 24);
    app.park_scroll_at(20);
    let parked = render_text(&app, 80, 24);
    assert_ne!(first, parked, "scrolling moved the viewport");

    assert!(app.enter_teammate_view());
    // The child renders and scrolls on its own scroll state.
    let _child = render_text(&app, 80, 24);
    app.scroll_transcript_up();
    assert_eq!(
        app.whole_scroll().top_offset(),
        20,
        "child scroll did not move the parent"
    );

    app.exit_teammate_view();
    let back = render_text(&app, 80, 24);
    assert_eq!(
        parked, back,
        "the parent viewport renders the same rows after the visit"
    );
}

/// Scrolling the child moves the child's scroll alone; the parent's raw top and
/// follow state never change while the child is on view.
#[test]
fn test_child_scroll_leaves_parent() {
    let mut app = app_with_parent(60);
    // A park names the row it stands on from the rows a draw built; without
    // the draw it keeps following the tail and the parent comparison is idle.
    drop(render_text(&app, 80, 24));
    app.park_scroll_at(15);
    let parent_top = app.whole_scroll().top_offset();
    let parent_follow = app.whole_scroll().is_following_tail();

    assert!(app.enter_teammate_view());
    // Step up three viewports and back one line: enough to break follow-tail
    // without returning the child to its tail.
    app.scroll_transcript_up();
    app.scroll_transcript_up();
    app.scroll_transcript_up();
    app.scroll_transcript_line_down(1);
    let view = app.ui.teammate_view.as_ref().expect("child on view");
    assert!(
        !view.scroll.is_following_tail(),
        "the child did scroll away"
    );

    assert_eq!(app.whole_scroll().top_offset(), parent_top);
    assert_eq!(
        app.whole_scroll().is_following_tail(),
        parent_follow,
        "the parent follow state is untouched by child scrolling"
    );
}

/// A child step that reaches the tail must not run the parent's tail work:
/// the trim and the new-message reset belong to the parent's own return to
/// the tail, which the reader never made. Clearing the unseen marker or
/// cutting rows from the child view would lose the parent's place.
#[test]
fn test_child_tail_spares_parent() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    for i in 0..60 {
        app.sessions
            .current
            .transcript
            .push_frame(user_frame(&format!("parent row {i}")));
    }
    app.rebuild_transcript();
    // Scroll away from the tail: this breaks follow-tail and starts the
    // parent's unseen count, the state a child-side tail step must not touch.
    // The step names the row it lands on from the rows a draw built.
    drop(render_text(&app, 80, 24));
    app.scroll_transcript_up();
    assert!(
        app.ui.transcript_view.unseen_since.is_some(),
        "the parent marked where the reader scrolled away"
    );
    app.sessions.current.transcript.push(subagent_line(40));
    let len_before = app.sessions.current.transcript.lines().len();
    let top_before = app.whole_scroll().top_offset();

    assert!(app.enter_teammate_view());
    app.scroll_transcript_up();
    app.scroll_transcript_down();
    app.scroll_transcript_follow_tail();
    let view = app.ui.teammate_view.as_ref().expect("child on view");
    assert!(
        view.scroll.is_following_tail(),
        "the child reached its tail"
    );

    assert_eq!(
        app.sessions.current.transcript.lines().len(),
        len_before,
        "no parent trim ran"
    );
    assert_eq!(
        app.whole_scroll().top_offset(),
        top_before,
        "the parent top is where the reader parked it"
    );
    assert!(
        app.ui.transcript_view.unseen_since.is_some(),
        "the parent's unseen marker survives the child's tail step"
    );
}

/// While a child is on view the parent's history loader stays off: hidden
/// history reads would spend disk and mutate parent state the reader cannot
/// see.
#[test]
fn test_child_blocks_parent_reads() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions
        .current
        .transcript
        .set_resident_byte_budget(4096);
    let text = "x".repeat(200);
    for _ in 0..40 {
        app.sessions
            .current
            .transcript
            .push_frame(user_frame(&text));
    }
    app.rebuild_transcript();
    app.rebuild_transcript();
    app.sessions.current.transcript.push(subagent_line(2));
    drop(render_text(&app, 80, 24));
    app.park_scroll_at(0);
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 4096,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows: Vec::new(),
        scan_steps: 0,
        scan_calls: AtomicU32::new(0),
    }));

    assert!(app.enter_teammate_view());
    for _ in 0..3 {
        app.load_older_frames();
    }
    assert!(
        !app.sessions.current.history_reads.holds_pending(),
        "no parent history read was dispatched while the child is on view"
    );
}

/// A parent read dispatched before the visit lands while the child is on
/// view. It belongs to the parent: the rows go into the parent transcript,
/// the parent viewport holds still, and the child render is untouched. New
/// dispatches stay blocked while the child is on view; an already-running
/// read completing is not a new dispatch and must not be lost either.
#[test]
fn test_parent_read_spares_child() {
    let mut app = app_with_parent(60);
    // Draw first: a park over rows that were never built keeps following the
    // tail, and the apply side drops every landed read as unwanted.
    drop(render_text(&app, 80, 24));
    app.park_scroll_at(20);
    let rows_before = pane_rows(&app);
    let (tx, rx) = mpsc::channel::<HistoryReadOutcome>();
    let front = app.sessions.current.transcript.frame_window_start();
    let disk_front = app.sessions.current.transcript.disk_front();
    drop(
        app.sessions
            .current
            .history_reads
            .dispatch(HistoryStamp::new(front, disk_front, None), rx),
    );

    assert!(app.enter_teammate_view());
    let child_before = render_text(&app, 80, 24);

    tx.send(HistoryReadOutcome::Rows(HistoryReadResult {
        rows: NamedRows::from_lines(vec![TranscriptLine::User("older parent row".into())]),
        anchor: 100,
    }))
    .ok();
    assert!(app.pump_history_read(), "the parent read landed");

    assert_eq!(
        app.sessions.current.transcript.disk_row_count(),
        1,
        "the rows went into the parent transcript"
    );
    assert_eq!(
        render_text(&app, 80, 24),
        child_before,
        "the child render is untouched by the parent read"
    );

    app.exit_teammate_view();
    // The row landed above the reader's, so the pane draws the same rows as
    // before the read: the viewport stands on the row it named.
    assert_eq!(
        pane_rows(&app),
        rows_before,
        "the parent still draws the rows the reader stood on"
    );
    app.park_scroll_at(0);
    assert!(
        render_text(&app, 80, 24).contains("older parent row"),
        "the row the read brought in is reachable by stepping back up"
    );
}

/// The display-row count follows the surface on view. Before the first child
/// render the published parent total must not answer for the child.
#[test]
fn test_active_total_switches() {
    let mut app = app_with_parent(60);
    let _parent = render_text(&app, 80, 24);
    let parent_total = app.transcript_display_rows();
    assert!(parent_total > 0, "the parent published a total");

    assert!(app.enter_teammate_view());
    let child_total = app.transcript_display_rows();
    assert_ne!(
        child_total, parent_total,
        "the count follows the surface on view, not the parent's published total"
    );
    // The child walks its own rows: 40 child lines plus the row spacers
    // between them, not the parent's count.
    assert_eq!(child_total, 79, "the count walks the child transcript");
}
