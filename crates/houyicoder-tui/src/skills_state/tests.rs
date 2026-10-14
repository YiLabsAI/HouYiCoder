use super::*;

fn request_id(value: u64) -> RequestId {
    RequestId(value)
}

#[test]
fn test_apply_detail_matches_request() {
    let mut state = SkillsPaneState::default();
    state.request_detail(request_id(7), "alpha".into());
    assert!(!state.apply_detail(request_id(8), Some("body".into())));
    assert!(state.apply_detail(request_id(7), Some("body".into())));
    assert_eq!(state.detail_body(), Some("body"));
}

#[test]
fn test_apply_detail_without_request() {
    let mut state = SkillsPaneState::default();
    assert!(!state.apply_detail(request_id(1), Some("body".into())));
    assert!(state.detail().is_none());
}

#[test]
fn test_scroll_clamps_to_body() {
    let mut state = SkillsPaneState::default();
    state.request_detail(request_id(1), "alpha".into());
    assert!(state.apply_detail(request_id(1), Some("body".into())));
    state.set_detail_max_offset(3);
    state.scroll_detail(10);
    assert_eq!(state.detail_offset(), Some(3), "Down stops at the last row");
    state.scroll_detail(-10);
    assert_eq!(state.detail_offset(), Some(0), "Up stops at the first row");
}

#[test]
fn test_shorter_body_resets_offset() {
    let mut state = SkillsPaneState::default();
    state.request_detail(request_id(1), "alpha".into());
    assert!(state.apply_detail(request_id(1), Some("body".into())));
    state.set_detail_max_offset(9);
    state.scroll_detail(9);
    assert_eq!(state.detail_offset(), Some(9));
    state.set_detail_max_offset(2);
    assert_eq!(
        state.detail_offset(),
        Some(2),
        "a shorter body pulls the view back"
    );
}

#[test]
fn test_cursor_moves_and_saturates() {
    let state = SkillsPaneState::default();
    state.move_cursor(-1, 5);
    assert_eq!(state.cursor(), 0, "Up saturates at the first row");
    state.move_cursor(2, 5);
    assert_eq!(state.cursor(), 2);
    state.move_cursor(10, 5);
    assert_eq!(state.cursor(), 4, "Down stops at the last row");
    state.move_cursor(3, 0);
    assert_eq!(state.cursor(), 0, "an empty list has no row to point at");
}

/// A failed send settles the loading detail with the body unavailable,
/// so the pane stops promising a reply that will never arrive.
#[test]
fn test_failed_send_settles_detail() {
    let mut state = SkillsPaneState::default();
    state.request_detail(request_id(4), "alpha".into());
    state.fail_detail(request_id(4));
    assert!(state.detail_body().is_none(), "the body stays unavailable");
    match state.detail() {
        Some(SkillDetail::Open { name, .. }) => assert_eq!(name, "alpha"),
        _ => panic!("expected an open detail"),
    }
}

/// A reply for a superseded request must not settle the current detail:
/// the later request is still pending.
#[test]
fn test_stale_failure_keeps_loading() {
    let mut state = SkillsPaneState::default();
    state.request_detail(request_id(5), "beta".into());
    state.fail_detail(request_id(4));
    assert!(
        matches!(state.detail(), Some(SkillDetail::Loading { .. })),
        "the current request keeps loading"
    );
}

/// A connection loss settles a loading detail but leaves an open one
/// alone: open is settled state, and the header stays useful.
#[test]
fn test_loss_settles_loading_only() {
    let mut state = SkillsPaneState::default();
    state.request_detail(request_id(1), "alpha".into());
    state.clear_pending();
    assert!(
        !matches!(state.detail(), Some(SkillDetail::Loading { .. })),
        "no reply is coming after the loss"
    );
    assert!(state.detail_body().is_none(), "the body stays unavailable");
    state.clear_pending();
    assert!(state.detail().is_some(), "an open detail survives");
}
