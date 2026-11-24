//! Tests for this module.

use super::*;

/// The wait doubles per attempt and stops at the ceiling; past the attempt
/// limit the policy answers None, which is the caller's failed state.
#[test]
fn test_delay_doubles_and_stops() {
    let policy = RestartPolicy::new(Duration::from_millis(1), Duration::from_millis(5), 3);
    assert_eq!(policy.delay(0), None, "no attempt means no wait");
    assert_eq!(policy.delay(1), Some(Duration::from_millis(1)));
    assert_eq!(policy.delay(2), Some(Duration::from_millis(2)));
    assert_eq!(policy.delay(3), Some(Duration::from_millis(4)));
    assert_eq!(policy.delay(4), None, "the ceiling ends restarts");
}

/// The cap holds however far the doubling would go.
#[test]
fn test_delay_caps() {
    let policy = RestartPolicy::new(Duration::from_millis(10), Duration::from_millis(15), 6);
    assert_eq!(policy.delay(2), Some(Duration::from_millis(15)));
    assert_eq!(policy.delay(5), Some(Duration::from_millis(15)));
}

/// A zero ceiling refuses every restart, which is the policy a caller uses to
/// disable automatic recovery.
#[test]
fn test_zero_attempts_disables() {
    let policy = RestartPolicy::new(Duration::from_millis(1), Duration::from_millis(5), 0);
    assert_eq!(policy.delay(1), None);
}
