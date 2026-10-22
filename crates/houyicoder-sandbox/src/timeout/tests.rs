//! The wall-clock message wording.

use super::message;

/// Seconds, never the raw millisecond count: a two-minute budget read as
/// 120000 seconds made a two-day limit look plausible.
#[test]
fn test_reports_seconds() {
    assert_eq!(message(120_000), "wall-clock 120s exceeded");
    assert_eq!(message(1_000), "wall-clock 1s exceeded");
}

/// A sub-second budget keeps its fraction. Truncating it to a whole second
/// rendered a nonzero limit as a measured zero.
#[test]
fn test_keeps_sub_second_budget() {
    assert_eq!(message(200), "wall-clock 0.2s exceeded");
    assert_eq!(message(1_500), "wall-clock 1.5s exceeded");
}
