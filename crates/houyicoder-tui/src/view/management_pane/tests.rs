//! The shared pane core's transitions: the reply that settles a pending
//! action, the settle-poll cadence and its budget, the cursor and level
//! grammar, and the scroll clamp. Each sibling pane's own tests cover its
//! row predicates and receipt wording on top of these.

use std::time::Instant;

use houyicoder_protocol::envelope::RequestId;

use super::PaneMachine;
use super::REFRESH_BUDGET;
use super::REFRESH_INTERVAL;

struct Row {
    settling: bool,
}

fn rows() -> Vec<Row> {
    vec![
        Row { settling: false },
        Row { settling: false },
        Row { settling: true },
    ]
}

fn machine() -> PaneMachine<Row, String> {
    PaneMachine::default()
}

/// A reply whose request does not match the pending one changes nothing, so
/// a late answer cannot overwrite a newer action's rows.
#[test]
fn test_stale_reply_ignored() {
    let mut pane = machine();
    pane.mark_pending(RequestId(7));
    let receipt = pane.apply_status(RequestId(8), rows(), |row| row.settling);
    assert!(receipt.is_none());
    assert!(pane.rows.is_empty(), "a stale reply changes nothing");
    assert!(pane.pending.is_some(), "the pending action stands");
}

/// A matching reply settles the pending action, hands back its receipt,
/// clamps the cursor, and arms one follow-up while a row keeps settling.
#[test]
fn test_apply_status_settles() {
    let mut pane = machine();
    pane.mark_pending_action(RequestId(7), Some("done".into()), false);
    pane.cursor = 9;
    let receipt = pane.apply_status(RequestId(7), rows(), |row| row.settling);
    assert_eq!(receipt.as_deref(), Some("done"));
    assert_eq!(pane.rows.len(), 3);
    assert_eq!(pane.cursor, 2, "the cursor clamps into the list");
    assert!(pane.pending.is_none());
    assert!(pane.refresh_armed(), "the settling row arms a follow-up");
}

/// An action whose reply is sent before its work finishes asks again even
/// though every row reads settled.
#[test]
fn test_expect_settle_arms() {
    let mut pane = machine();
    pane.mark_pending_action(RequestId(9), None, true);
    let settled = vec![Row { settling: false }];
    pane.apply_status(RequestId(9), settled, |row| row.settling);
    assert!(pane.refresh_armed());
}

/// The follow-up fires only when due, and one action spends at most the
/// budget of asks so a wedged row is not polled forever.
#[test]
fn test_refresh_budget() {
    let mut pane = machine();
    pane.mark_pending(RequestId(1));
    pane.apply_status(RequestId(1), rows(), |row| row.settling);
    let now = Instant::now();
    assert!(!pane.take_due_refresh(now), "the interval is not spent yet");
    let mut later = now + REFRESH_INTERVAL;
    for _ in 0..REFRESH_BUDGET {
        assert!(pane.take_due_refresh(later));
        later += REFRESH_INTERVAL;
        pane.mark_settling_pending(RequestId(2));
        pane.apply_status(RequestId(2), rows(), |row| row.settling);
    }
    assert!(!pane.refresh_armed(), "the budget is spent");
    assert!(!pane.take_due_refresh(later));
}

/// An error reply settles the pane's own request and keeps the rows.
#[test]
fn test_fail_pending_keeps_rows() {
    let mut pane = machine();
    pane.mark_pending(RequestId(3));
    pane.apply_status(RequestId(3), rows(), |row| row.settling);
    pane.mark_pending_action(RequestId(4), Some("unused".into()), false);
    assert!(pane.fail_pending(RequestId(4)));
    assert!(pane.pending.is_none());
    assert_eq!(pane.rows.len(), 3);
    assert!(
        !pane.fail_pending(RequestId(4)),
        "a second settle is not the pane's"
    );
}

/// The cursor clamps at both ends and collapses to zero on an empty list;
/// the detail opens only with a row and returns without disturbing rows.
#[test]
fn test_cursor_and_levels() {
    let mut pane = machine();
    pane.mark_pending(RequestId(1));
    pane.apply_status(RequestId(1), rows(), |row| row.settling);
    pane.move_cursor(-5);
    assert_eq!(pane.cursor, 0);
    pane.move_cursor(5);
    assert_eq!(pane.cursor, 2);
    pane.open_detail();
    assert_eq!(pane.level, 1);
    pane.close_detail();
    assert_eq!(pane.level, 0);
    assert_eq!(pane.rows.len(), 3);
    pane.rows.clear();
    pane.move_cursor(1);
    assert_eq!(pane.cursor, 0);
    pane.open_detail();
    assert_eq!(pane.level, 0, "an empty list has nothing to open");
}

/// The detail scroll clamps to the maximum the render published, and a
/// shorter body pulls the offset down with it.
#[test]
fn test_scroll_clamps() {
    let pane = machine();
    pane.set_detail_max_offset(10);
    pane.scroll_detail(6);
    assert_eq!(pane.detail_offset(), 6);
    pane.scroll_detail(6);
    assert_eq!(pane.detail_offset(), 10);
    pane.scroll_detail(-100);
    assert_eq!(pane.detail_offset(), 0);
    pane.scroll_detail(8);
    pane.set_detail_max_offset(3);
    assert_eq!(pane.detail_offset(), 3);
}

/// A session switch clears everything, so the next session starts from its
/// own first reply.
#[test]
fn test_reset_clears() {
    let mut pane = machine();
    pane.mark_pending(RequestId(1));
    pane.apply_status(RequestId(1), rows(), |row| row.settling);
    pane.cursor = 2;
    pane.level = 1;
    pane.scroll_detail(4);
    pane.reset();
    assert!(pane.rows.is_empty());
    assert!(pane.pending.is_none());
    assert_eq!(pane.cursor, 0);
    assert_eq!(pane.level, 0);
    assert_eq!(pane.detail_offset(), 0);
    assert!(!pane.refresh_armed());
}
