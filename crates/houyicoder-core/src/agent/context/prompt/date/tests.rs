//! Tests for this module.

use super::*;

#[test]
fn test_civil_epoch() {
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(-1), (1969, 12, 31));
}

/// Leap-day boundaries: 2024-02-29 is day 19782 and 2024-03-01 is 19783.
#[test]
fn test_civil_leap_boundaries() {
    assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    assert_eq!(civil_from_days(19_783), (2024, 3, 1));
}

#[test]
fn test_iso_date_format() {
    assert_eq!(iso_date(0), "1970-01-01");
    assert_eq!(iso_date(86_400), "1970-01-02");
}

/// The rendered line is the shape both prompt builders depend on: a fixed
/// lead-in and a zero-padded civil date. Callers assert against this line
/// verbatim, so the shape needs its own guard.
#[test]
fn test_utc_line_shape() {
    let line = utc_date_line();
    let date = line
        .strip_prefix("Today's date is ")
        .and_then(|rest| rest.strip_suffix('.'))
        .expect("date line is a fixed lead-in plus a trailing period");
    let parts: Vec<&str> = date.split('-').collect();
    assert_eq!(parts.len(), 3, "{line}");
    assert_eq!(parts[0].len(), 4, "{line}");
    assert_eq!(parts[1].len(), 2, "{line}");
    assert_eq!(parts[2].len(), 2, "{line}");
    assert!(
        parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())),
        "{line}"
    );
}
