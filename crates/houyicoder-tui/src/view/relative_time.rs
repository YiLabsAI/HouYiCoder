//! Shared time formatting: a relative stamp for an epoch-second timestamp, and
//! a span for a duration. Pure functions (now passed in, not read from
//! SystemTime) so tests are deterministic and the display does not drift
//! between redraws. The caller computes now from SystemTime at its call site.

/// Format an epoch-second timestamp as a short relative string: "just
/// now", "5m ago", "3h ago", "2d ago", "1w ago". saturating_sub guards
/// against a clock set before the epoch. Returns "never" when epoch is
/// 0 (the never-invoked sentinel).
pub(crate) fn relative_time(now: u64, epoch: u64) -> String {
    if epoch == 0 {
        return "never".to_string();
    }
    let elapsed = now.saturating_sub(epoch);
    if elapsed < 60 {
        "just now".to_string()
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{}h ago", elapsed / 3600)
    } else if elapsed < 604800 {
        format!("{}d ago", elapsed / 86400)
    } else {
        format!("{}w ago", elapsed / 604800)
    }
}

/// A span of time as a compact display string: milliseconds below a second,
/// tenths of a second below a minute, then whole minutes, hours, and days with
/// the next unit down. A turn that ran for hours reads as a duration rather
/// than as a six-figure second count.
pub(crate) fn format_span_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format_span_secs(ms / 1000)
    }
}

/// A span already counted in whole seconds, for a session total: seconds below
/// a minute, then whole minutes, hours, and days, each carrying the next unit
/// down. Days keep only hours, because a session read at the day scale does
/// not need its minutes.
pub(crate) fn format_span_secs(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = secs / 3600 % 24;
    let mins = secs / 60 % 60;
    let rem = secs % 60;
    match (days, hours, mins) {
        (0, 0, 0) => format!("{rem}s"),
        (0, 0, m) if rem == 0 => format!("{m}m"),
        (0, 0, m) => format!("{m}m {rem}s"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// Read the current epoch seconds from SystemTime. Centralized so both
/// callers (worktree_pane, skills_pane) share the same clock-read path.
pub(crate) fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
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
}
