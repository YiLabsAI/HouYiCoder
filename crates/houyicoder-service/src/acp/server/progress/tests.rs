//! Tests for the tool progress throttle.

use std::time::Duration;
use std::time::Instant;

use super::ToolProgressThrottle;

/// The first update for a call passes, and one inside the interval after it
/// is held back.
#[test]
fn test_progress_throttles_window() {
    let mut throttle = ToolProgressThrottle::new();
    let start = Instant::now();
    assert!(throttle.allow("call-1", start));
    assert!(!throttle.allow("call-1", start + Duration::from_millis(10)));
}

/// A gap at the interval lets the next update through.
#[test]
fn test_progress_allows_interval() {
    let mut throttle = ToolProgressThrottle::new();
    let start = Instant::now();
    assert!(throttle.allow("call-1", start));
    assert!(throttle.allow("call-1", start + Duration::from_millis(50)));
}

/// Two calls report on their own schedules, so one call's update does not
/// hold back another's.
#[test]
fn test_progress_tracks_calls_separately() {
    let mut throttle = ToolProgressThrottle::new();
    let start = Instant::now();
    assert!(throttle.allow("call-1", start));
    assert!(throttle.allow("call-2", start));
}

/// A new turn starts unthrottled even when the same call id reported just
/// before it.
#[test]
fn test_progress_reset_clears_window() {
    let mut throttle = ToolProgressThrottle::new();
    let start = Instant::now();
    assert!(throttle.allow("call-1", start));
    throttle.reset();
    assert!(throttle.allow("call-1", start + Duration::from_millis(1)));
}
