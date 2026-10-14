//! Pure-logic tests for the shared selection free functions. The cross-layer
//! gesture tests (drive handle_mouse over a real render) live in
//! src/view/pane_select_tests.rs and src/view/drag_select_tests.rs.

use std::time::Instant;

use crate::brief::tool_call_brief;
use crate::composition;
use crate::records::{ToolOutcome, TranscriptLine};
use crate::selection::Selection;
use crate::selection::region::{RegionIntent, clear_stale_click, edge_scroll_if_at_edge};
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

/// A click-only drag (anchor == focus, no span) is stale and is cleared before
/// a scroll so the edge auto-scroll does not extend a one-cell selection the
/// user never moved. A real drag (anchor != focus) survives so the user can
/// scroll while holding an active selection.
#[test]
fn test_clear_stale_click_only() {
    // Stale click-only: drag marked active but never moved.
    let mut sel = sel_with((5, 5), (5, 5));
    clear_stale_click(&mut sel);
    assert!(!sel.is_dragging, "stale click cleared");
    assert!(!sel.has_selection());

    // A real drag must survive a scroll.
    let mut sel = sel_with((5, 5), (5, 8));
    clear_stale_click(&mut sel);
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
    app.whole_scroll_mut().jump_to(5);
    let start = app.whole_scroll().top_offset();

    app.apply_region_intent(RegionIntent::ScrollUp);
    assert_eq!(app.whole_scroll().top_offset(), start.saturating_sub(1));

    app.apply_region_intent(RegionIntent::ScrollDown);
    assert_eq!(app.whole_scroll().top_offset(), start);
}

/// A copy intent raises the notice for the text the release copied: the leaf
/// writes the clipboard and names the effect, the router shows it.
#[test]
fn test_copied_raises_notice() {
    let mut app = rendered_working();

    app.apply_region_intent(RegionIntent::Copied("hi".into()));

    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("copied 2 chars"),
        "the copy notice is shown:\n{out}"
    );
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
