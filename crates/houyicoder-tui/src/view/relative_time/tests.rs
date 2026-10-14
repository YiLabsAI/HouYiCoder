use super::*;

#[test]
fn test_never() {
    assert_eq!(relative_time(1000, 0), "never");
}

#[test]
fn test_just_now() {
    assert_eq!(relative_time(1000, 950), "just now");
}

#[test]
fn test_minutes() {
    assert_eq!(relative_time(1000, 900), "1m ago");
    assert_eq!(relative_time(1000, 700), "5m ago");
}

#[test]
fn test_hours() {
    assert_eq!(relative_time(10000, 6400), "1h ago");
}

#[test]
fn test_days() {
    assert_eq!(relative_time(200000, 113600), "1d ago");
}

#[test]
fn test_weeks() {
    assert_eq!(relative_time(2000000, 2000), "3w ago");
}

#[test]
fn test_span_ms_tiers() {
    assert_eq!(format_span_ms(0), "0ms");
    assert_eq!(format_span_ms(620), "620ms");
    assert_eq!(format_span_ms(1000), "1.0s");
    assert_eq!(format_span_ms(3200), "3.2s");
    assert_eq!(format_span_ms(59_900), "59.9s");
    assert_eq!(format_span_ms(60_000), "1m");
    assert_eq!(format_span_ms(318_000), "5m 18s");
}

#[test]
fn test_span_secs_tiers() {
    assert_eq!(format_span_secs(0), "0s");
    assert_eq!(format_span_secs(42), "42s");
    assert_eq!(format_span_secs(91), "1m 31s");
    assert_eq!(format_span_secs(600), "10m");
    assert_eq!(format_span_secs(3600), "1h");
    assert_eq!(format_span_secs(19_080), "5h 18m");
    assert_eq!(format_span_secs(86_400), "1d");
    assert_eq!(
        format_span_secs(1_032_337),
        "11d 22h",
        "a session of days is a duration, not a second count"
    );
}

#[test]
fn test_clock_before_epoch_saturates() {
    // now < epoch (clock set before 1970): saturating_sub yields 0,
    // which reads as "just now" — no panic, no underflow.
    assert_eq!(relative_time(0, 1000), "just now");
}

#[test]
fn test_session_span_label() {
    assert_eq!(session_span_label(None), "—");
    assert_eq!(session_span_label(Some(0)), "0ms");
    assert_eq!(session_span_label(Some(620)), "620ms");
    assert_eq!(session_span_label(Some(999)), "999ms");
    assert_eq!(session_span_label(Some(1000)), "1s");
    assert_eq!(
        session_span_label(Some(91_400)),
        "1m 31s",
        "a session past a minute reads as minutes and seconds"
    );
}

/// The summary row rounds to the nearest second above its step instead of
/// truncating, and the values either side of a half second pin it.
#[test]
fn test_turn_span_label_rounds() {
    assert_eq!(turn_span_label(0), "0ms");
    assert_eq!(turn_span_label(620), "620ms");
    assert_eq!(turn_span_label(999), "999ms");
    assert_eq!(turn_span_label(1000), "1s");
    assert_eq!(turn_span_label(1499), "1s");
    assert_eq!(turn_span_label(1500), "2s");
}
