use super::*;

/// The cursor steps within the list and stops at both ends: an arrow held
/// past a bound must not wrap to the other end of the pane.
#[test]
fn test_cursor_steps_and_clamps() {
    let state = AgentsPaneState::default();
    assert_eq!(state.cursor(), 0, "a fresh pane points at the first row");
    state.move_cursor(-1, 3);
    assert_eq!(state.cursor(), 0, "the first row is the upper bound");
    state.move_cursor(1, 3);
    assert_eq!(state.cursor(), 1);
    state.move_cursor(1, 3);
    state.move_cursor(1, 3);
    assert_eq!(state.cursor(), 2, "the last row is the lower bound");
}

/// An empty list has no row to point at, so any move parks the cursor at the
/// first index and a later rebuild starts from there.
#[test]
fn test_empty_list_parks_cursor() {
    let state = AgentsPaneState::default();
    state.move_cursor(1, 3);
    state.move_cursor(1, 0);
    assert_eq!(state.cursor(), 0);
}

/// A rebuild that shortened the list pulls the cursor back inside it, while a
/// rebuild that kept it long enough leaves the user's row alone.
#[test]
fn test_clamp_pulls_cursor_back() {
    let state = AgentsPaneState::default();
    state.move_cursor(1, 3);
    state.move_cursor(1, 3);
    state.clamp_cursor(3);
    assert_eq!(state.cursor(), 2, "a cursor still in range does not move");
    state.clamp_cursor(1);
    assert_eq!(
        state.cursor(),
        0,
        "a cursor past the end lands on the last row"
    );
    state.clamp_cursor(0);
    assert_eq!(state.cursor(), 0, "an emptied list parks the cursor");
}

/// Opening a detail starts at its first line: a type opened after a scrolled
/// one must not inherit the earlier position.
#[test]
fn test_detail_opens_at_top() {
    let mut state = AgentsPaneState::default();
    state.open_detail("explore".into());
    state.set_detail_max_offset(20);
    state.scroll_detail(5);
    assert_eq!(state.detail_offset(), 5);
    state.open_detail("general-purpose".into());
    assert_eq!(state.detail_offset(), 0);
    assert_eq!(
        state.detail().map(|detail| detail.subagent_type()),
        Some("general-purpose")
    );
}

/// The offset cannot pass either end of the body the last frame measured, and
/// a shorter body pulls a scrolled view back to the top.
#[test]
fn test_scroll_stays_within_body() {
    let mut state = AgentsPaneState::default();
    state.open_detail("explore".into());
    state.set_detail_max_offset(3);
    state.scroll_detail(10);
    assert_eq!(state.detail_offset(), 3, "the last line is the lower bound");
    state.scroll_detail(-1);
    assert_eq!(state.detail_offset(), 2);
    state.scroll_detail(-5);
    assert_eq!(
        state.detail_offset(),
        0,
        "the first line is the upper bound"
    );
    state.scroll_detail(2);
    state.set_detail_max_offset(1);
    assert_eq!(
        state.detail_offset(),
        1,
        "a shorter body pulls the view back"
    );
}

/// Scroll keys with no detail open do nothing: the list owns the arrows while
/// the detail is closed.
#[test]
fn test_scroll_without_detail_noop() {
    let state = AgentsPaneState::default();
    state.scroll_detail(5);
    assert_eq!(state.detail_offset(), 0);
    state.set_detail_max_offset(10);
    assert_eq!(state.detail_offset(), 0);
}

/// Closing the detail returns the pane to the list and drops the scroll
/// position with it.
#[test]
fn test_close_detail_clears_position() {
    let mut state = AgentsPaneState::default();
    state.open_detail("explore".into());
    state.set_detail_max_offset(5);
    state.scroll_detail(4);
    state.close_detail();
    assert!(state.detail().is_none());
    assert_eq!(state.detail_offset(), 0);
    state.open_detail("explore".into());
    assert_eq!(state.detail_offset(), 0);
}
