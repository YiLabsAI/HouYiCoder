//! Tests for this module.

use super::*;

#[test]
fn test_calls_reset_on_begin() {
    let mut turn = UserTurn::default();
    assert_eq!(turn.spend_call(), 1);
    assert_eq!(turn.spend_call(), 2);
    assert_eq!(turn.calls(), 2);
    turn.begin();
    assert_eq!(turn.calls(), 0, "a new turn starts with no calls spent");
}

/// The legs sum, so the leg that finishes a turn reports what the turn
/// spent rather than what that leg alone took.
#[test]
fn test_worked_sums_across_legs() {
    let mut turn = UserTurn::default();
    assert_eq!(turn.take_worked(), None, "no leg ran, so work is unknown");
    turn.account(Duration::from_secs(2));
    turn.account(Duration::from_secs(3));
    assert_eq!(turn.take_worked(), Some(Duration::from_secs(5)));
    assert_eq!(turn.take_worked(), None, "taking the work leaves none");
    turn.account(Duration::from_secs(7));
    turn.begin();
    assert_eq!(turn.take_worked(), None, "a new turn starts unmeasured");
}
