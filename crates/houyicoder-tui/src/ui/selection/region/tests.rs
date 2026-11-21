//! Pure-logic tests for the shared selection free functions. The cross-layer
//! gesture tests drive handle_mouse over a real render, and each lives beside
//! the view whose rows it acts on.

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Color;

use crate::brief::tool_call_brief;
use crate::composition;
use crate::records::{ToolOutcome, TranscriptLine};
use crate::selection::region::{
    RegionIntent, clear_stale_click, edge_scroll_if_at_edge, paint_overlay,
};
use crate::selection::{
    CopyReceipt, CopyReport, Selection, TAG_DIFF_BORDER, TAG_DIFF_HUNK, TAG_PLAIN, snap_to_glyphs,
};
use crate::state::App;
use crate::test_harness::{render_text, working_app};

fn sel_with(anchor: (u16, usize), focus: (u16, usize)) -> Selection {
    Selection {
        is_dragging: true,
        anchor: Some(anchor),
        focus: Some(focus),
        span_origin: None,
        ..Default::default()
    }
}

/// A drag that crosses a diff's border and hunk gap keeps one continuous
/// highlight: those rows carry no copyable text, but they are still painted.
/// Copying them out is the extraction step's job, not the paint step's.
#[test]
fn test_overlay_covers_diff() {
    let rect = Rect::new(0, 0, 10, 4);
    let mut buf = Buffer::empty(rect);
    let rows = vec![
        (TAG_PLAIN, "one".to_string()),
        (TAG_DIFF_BORDER, String::new()),
        (TAG_DIFF_HUNK, "...".to_string()),
        (TAG_PLAIN, "two".to_string()),
    ];
    let sel = sel_with((0, 0), (3, 3));
    paint_overlay(&mut buf, rect, &rows, &sel, 0);
    for y in 0..4 {
        let cell = buf.cell(Position::new(1, y)).expect("cell in range");
        assert_eq!(
            cell.bg,
            Color::Indexed(24),
            "row {y} is painted as part of the block"
        );
    }
}

/// The paint widens a range edge that falls inside a wide glyph, so the
/// highlight covers exactly the glyphs the extractor takes. A half-tinted wide
/// glyph would tell the user one character is selected while the clipboard
/// receives the whole one.
#[test]
fn test_overlay_covers_wide_glyph() {
    let rect = Rect::new(0, 0, 10, 1);
    let mut buf = Buffer::empty(rect);
    // Three wide characters: columns 0-1, 2-3, 4-5.
    let rows = vec![(TAG_PLAIN, "\u{4e00}\u{4e8c}\u{4e09}".to_string())];
    // The range starts on the second half of the first glyph and ends on the
    // first half of the third.
    let sel = sel_with((1, 0), (4, 0));
    paint_overlay(&mut buf, rect, &rows, &sel, 0);
    let sel_bg = Color::Indexed(24);
    for x in 0..6 {
        let cell = buf.cell(Position::new(x, 0)).expect("cell");
        assert_eq!(
            cell.bg, sel_bg,
            "column {x} belongs to a touched glyph and must be painted"
        );
    }
    let outside = buf.cell(Position::new(6, 0)).expect("cell past the glyphs");
    assert_ne!(outside.bg, sel_bg, "an untouched column stays unpainted");
}

/// The widening step must stay inside the row it is given, whatever range it
/// receives. A range that returned an end before its start would make the
/// paint loop empty, and one that reached past the row would paint cells the
/// row does not own.
#[test]
fn test_glyph_widen_in_bounds() {
    // Empty row, range entirely past the row, empty range, and a trailing wide
    // glyph half-covered.
    for (row, c0, c1) in [
        ("", 0usize, 5usize),
        ("abc", 10, 20),
        ("\u{4e00}\u{4e8c}", 0, 0),
        ("abc\u{4e00}", 1, 3),
    ] {
        let (s0, s1) = snap_to_glyphs(row, c0, c1);
        assert!(
            s0 <= s1,
            "{row:?} ({c0}, {c1}) widened to a backwards range {s0}..{s1}"
        );
        assert!(
            s1 <= c1.max(s0),
            "{row:?} ({c0}, {c1}) widened past what it was given: {s0}..{s1}"
        );
    }
}

/// A click-only drag (anchor == focus, no span) is stale and is cleared before
/// a scroll so the edge auto-scroll does not extend a one-cell selection the
/// user never moved. A real drag (anchor != focus) survives so the user can
/// scroll while holding an active selection.
#[test]
fn test_clear_stale_click_only() {
    // Stale click-only: drag marked active but never moved.
    let mut sel = sel_with((5, 5), (5, 5));
    assert!(
        clear_stale_click(&mut sel),
        "erasing the selection is a change the screen shows"
    );
    assert!(!sel.is_dragging, "stale click cleared");
    assert!(!sel.has_selection());

    // A real drag must survive a scroll.
    let mut sel = sel_with((5, 5), (5, 8));
    assert!(
        !clear_stale_click(&mut sel),
        "a real drag leaves the screen as it was"
    );
    assert!(sel.is_dragging, "real drag preserved");
    assert!(sel.has_selection());
}

/// The edge auto-scroll poll does nothing without an active transcript drag,
/// so a bare poll tick never scrolls the transcript.
#[test]
fn test_edge_scroll_requires_drag() {
    let mut app = composition::app();
    assert!(!edge_scroll_if_at_edge(&mut app));
}

/// The edge auto-scroll poll does nothing when a drag is active but the cursor
/// was never published, so there is no edge coordinate to scroll from.
#[test]
fn test_edge_scroll_requires_cursor() {
    let mut app = composition::app();
    app.ui.selection.transcript.start(0, 0);
    assert!(!edge_scroll_if_at_edge(&mut app));
}

/// A working screen with one tool call whose rows publish a fold group, so
/// the toggle intents have something to act on.
fn rendered_working() -> App {
    let mut app = working_app();
    app.sessions.current.transcript = vec![
        TranscriptLine::User("go".into()),
        TranscriptLine::Tool {
            name: "bash".into(),
            tool: "bash".into(),
            status: tool_call_brief("bash", &serde_json::json!({ "command": "echo hi" })),
            invocation: "echo hi".into(),
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            background_task: None,
            body: "hi".into(),
            is_diff: false,
        },
    ]
    .into();
    let _out = render_text(&app, 80, 24);
    app
}

/// The two scroll intents move the transcript one line each, which is what the
/// edge of a drag asks for.
#[test]
fn test_scroll_intents_move() {
    let mut app = rendered_working();
    app.park_scroll_at(5);
    let start = app.whole_scroll().top_offset();

    app.apply_region_intent(RegionIntent::ScrollUp);
    assert_eq!(app.whole_scroll().top_offset(), start.saturating_sub(1));

    app.apply_region_intent(RegionIntent::ScrollDown);
    assert_eq!(app.whole_scroll().top_offset(), start);
}

/// A copy intent starts the write and claims nothing: the notice appears
/// only when the background write reports, and then carries its outcome.
#[test]
fn test_copied_raises_notice() {
    let mut app = rendered_working();

    app.apply_region_intent(RegionIntent::Copied);
    assert!(
        !app.poll_copy_results(),
        "the intent alone raises nothing; the write has not reported yet"
    );
    let before = render_text(&app, 80, 24);
    assert!(
        !before.contains("chars"),
        "no notice is shown before the write reports:\n{before}"
    );
    app.ui.notifications.copy_receipts.send(CopyReceipt {
        chars: 2,
        outcome: CopyReport::NativeCopied,
    });
    app.poll_copy_results();

    let out = render_text(&app, 80, 24);
    assert!(out.contains("2 chars"), "the copy notice is shown:\n{out}");
}

/// A failed write still reports: the notice says the copy failed rather
/// than staying silent about it.
#[test]
fn test_failed_copy_reports() {
    let mut app = rendered_working();
    app.ui.notifications.copy_receipts.send(CopyReceipt {
        chars: 3,
        outcome: CopyReport::CopyFailed,
    });
    app.poll_copy_results();
    let out = render_text(&app, 80, 24);
    assert!(out.contains("copy failed"), "the failure is shown:\n{out}");
}

/// A toggle intent clears the click it consumed, so the next press is not read
/// as a repeat of the same click.
#[test]
fn test_toggle_clears_click() {
    let mut app = rendered_working();
    app.ui.selection.transcript.anchor = Some((0, 0));
    app.ui.selection.transcript.focus = Some((0, 0));
    app.ui.selection.transcript.last_click = Some((Instant::now(), 0, 0, 1));

    app.apply_region_intent(RegionIntent::ToggleFold(0));

    assert!(
        app.ui.selection.transcript.anchor.is_none(),
        "the click it consumed is cleared"
    );
    assert!(app.ui.selection.transcript.last_click.is_none());
}

/// A clean click whose collapse finds nothing expanded falls through to the
/// release, which clears the click-only caret. This is the branch a plain
/// click on non-toggleable content takes.
#[test]
fn test_collapse_falls_through() {
    let mut app = rendered_working();
    app.ui.selection.transcript.anchor = Some((0, 0));
    app.ui.selection.transcript.focus = Some((0, 0));
    app.ui.selection.transcript.is_dragging = true;

    app.apply_region_intent(RegionIntent::CollapseUnderAnchor);

    assert!(
        !app.ui.selection.transcript.has_selection(),
        "the release cleared the click-only caret"
    );
}

/// A thinking toggle on a row that offers no block declines and falls through
/// to the collapse, which falls through to the release.
#[test]
fn test_thinking_falls_through() {
    let mut app = rendered_working();
    app.ui.selection.transcript.anchor = Some((0, 0));
    app.ui.selection.transcript.focus = Some((0, 0));
    app.ui.selection.transcript.is_dragging = true;

    app.apply_region_intent(RegionIntent::ToggleThinking(0));

    assert!(
        !app.ui.selection.transcript.has_selection(),
        "a declined toggle still releases the click"
    );
}
