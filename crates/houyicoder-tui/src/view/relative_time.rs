//! Shared relative-time formatting for epoch-second timestamps. Pure
//! function (now passed in, not read from SystemTime) so tests are
//! deterministic and the display does not drift between redraws. The
//! caller computes now from SystemTime at its call site.

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
    fn test_clock_before_epoch_saturates() {
        // now < epoch (clock set before 1970): saturating_sub yields 0,
        // which reads as "just now" — no panic, no underflow.
        assert_eq!(relative_time(0, 1000), "just now");
    }
}
