//! The run's current date, rendered as prompt grounding text. Day-granular
//! and UTC, so a long session renders the same line until the day rolls over.
//!
//! Both prompt builders carry it, past the byte-stable part each one caches:
//! the parent appends the line to the system prompt after the dynamic
//! boundary, and the child puts it in its env block. Neither puts it in a
//! message body, because a saved memory must quote text the session log
//! holds, and a quote the model can copy from a message has to be verifiable.

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Today's date line. UTC, not local: the two differ only inside the
/// local-midnight-to-UTC-midnight window, immaterial for date-grounding a
/// coding run, and staying UTC keeps a timezone dependency out of this crate.
pub(crate) fn utc_date_line() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("Today's date is {}.", iso_date(secs))
}

/// Seconds since the Unix epoch as a zero-padded civil date.
fn iso_date(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since the Unix epoch to a civil (y, m, d) triple. The shifted-era
/// form keeps the arithmetic valid across the epoch boundary (negative day
/// counts included) without a calendar dependency.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests;
