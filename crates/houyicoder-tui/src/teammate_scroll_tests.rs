//! Parent and child scroll isolation: the parent's scroll position survives
//! a visit into a teammate view, and the child's scrolling never runs the
//! parent's trim or new-message side effects.

use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use crate::records::TranscriptLine;
use crate::state::App;
use crate::test_harness::MockSnapshot;
use crate::transcript::TranscriptFrame;
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
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.runtime = Some(crate::composition::shared_runtime());
    for i in 0..parent_rows {
        app.transcript
            .push_frame(user_frame(&format!("parent row {i}")));
    }
    app.rebuild_transcript();
    app.transcript.push(subagent_line(40));
    app
}

/// Entering the child view starts the child scroll at the tail and leaves the
/// parent scroll where the reader parked it, so the exit renders the same
/// parent rows again.
#[test]
fn test_parent_scroll_survives_visit() {
    let mut app = app_with_parent(60);
    let first = crate::test_harness::render_text(&app, 80, 24);
    app.transcript_scroll.jump_to(20);
    let parked = crate::test_harness::render_text(&app, 80, 24);
    assert_ne!(first, parked, "scrolling moved the viewport");

    assert!(app.enter_teammate_view());
    // The child renders and scrolls on its own scroll state.
    let _child = crate::test_harness::render_text(&app, 80, 24);
    app.scroll_transcript_up();
    assert_eq!(
        app.transcript_scroll.raw_top(),
        20,
        "child scroll did not move the parent"
    );

    app.exit_teammate_view();
    let back = crate::test_harness::render_text(&app, 80, 24);
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
    app.transcript_scroll.jump_to(15);
    let parent_top = app.transcript_scroll.raw_top();
    let parent_follow = app.transcript_scroll.is_following_tail();

    assert!(app.enter_teammate_view());
    // Step up three viewports and back one line: enough to break follow-tail
    // without returning the child to its tail.
    app.scroll_transcript_up();
    app.scroll_transcript_up();
    app.scroll_transcript_up();
    app.scroll_transcript_line_down(1);
    let view = app.teammate_view.as_ref().expect("child on view");
    assert!(
        !view.scroll.is_following_tail(),
        "the child did scroll away"
    );

    assert_eq!(app.transcript_scroll.raw_top(), parent_top);
    assert_eq!(
        app.transcript_scroll.is_following_tail(),
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
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.runtime = Some(crate::composition::shared_runtime());
    for i in 0..60 {
        app.transcript
            .push_frame(user_frame(&format!("parent row {i}")));
    }
    app.rebuild_transcript();
    // Scroll away from the tail: this breaks follow-tail and starts the
    // parent's unseen count, the state a child-side tail step must not touch.
    app.scroll_transcript_up();
    assert!(
        app.unseen_since.is_some(),
        "the parent marked where the reader scrolled away"
    );
    app.transcript.push(subagent_line(40));
    let len_before = app.transcript.lines().len();
    let top_before = app.transcript_scroll.raw_top();

    assert!(app.enter_teammate_view());
    app.scroll_transcript_up();
    app.scroll_transcript_down();
    app.scroll_transcript_follow_tail();
    let view = app.teammate_view.as_ref().expect("child on view");
    assert!(
        view.scroll.is_following_tail(),
        "the child reached its tail"
    );

    assert_eq!(
        app.transcript.lines().len(),
        len_before,
        "no parent trim ran"
    );
    assert_eq!(
        app.transcript_scroll.raw_top(),
        top_before,
        "the parent top is where the reader parked it"
    );
    assert!(
        app.unseen_since.is_some(),
        "the parent's unseen marker survives the child's tail step"
    );
}

/// While a child is on view the parent's history loader stays off: hidden
/// history reads would spend disk and mutate parent state the reader cannot
/// see.
#[test]
fn test_child_blocks_parent_reads() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.runtime = Some(crate::composition::shared_runtime());
    app.transcript.set_resident_byte_budget(4096);
    let text = "x".repeat(200);
    for _ in 0..40 {
        app.transcript.push_frame(user_frame(&text));
    }
    app.rebuild_transcript();
    app.rebuild_transcript();
    app.transcript.push(subagent_line(2));
    app.transcript_scroll.jump_to(0);
    app.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 4096,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows: Vec::new(),
        index_steps: 0,
        index_calls: AtomicU32::new(0),
    }));

    assert!(app.enter_teammate_view());
    for _ in 0..3 {
        app.load_older_frames();
    }
    assert!(
        !app.transcript.history_read_pending(),
        "no parent history read was dispatched while the child is on view"
    );
}

/// The display-row count follows the surface on view. Before the first child
/// render the published parent total must not answer for the child.
#[test]
fn test_active_total_switches() {
    let mut app = app_with_parent(60);
    let _parent = crate::test_harness::render_text(&app, 80, 24);
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
