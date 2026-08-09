//! Pure-logic tests for the shared selection free functions. The cross-layer
//! gesture tests (drive handle_mouse over a real render) live in
//! src/view/pane_select_tests.rs and src/view/drag_select_bug_tests.rs.

use crate::selection::Selection;
use crate::selection::surface::clear_stale_click;

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
