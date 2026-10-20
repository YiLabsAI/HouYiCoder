//! The timeline's time axis and the bars drawn on it.
//!
//! The axis is the active span of a turn: the records' working intervals with
//! idle gaps folded, so a long wait does not squeeze the work into one column.
//! Bars are drawn in eighth-block resolution so a short call and a long one
//! stay distinguishable.

use super::*;

/// The longest idle gap the axis keeps. A turn can sit on a permission prompt
/// for tens of minutes; the axis describes the work, so a longer gap is folded
/// to this and the active time keeps its resolution.
const IDLE_CAP_MS: u64 = 2_000;

/// A monotonic map from a turn's own clock to the time it spent working.
///
/// The records' own spans are kept whole and the idle time between them is
/// capped, so a long wait on a permission prompt does not squeeze the work
/// into the first column. Concurrent calls overlap in the source clock and
/// keep overlapping here.
pub(crate) struct ActiveAxis {
    /// Working intervals, merged, as (source start, source end, mapped start).
    segments: Vec<(u64, u64, u64)>,
    total: u64,
}

impl ActiveAxis {
    pub(crate) fn new(records: &[TrajectoryRecord]) -> Self {
        let mut spans: Vec<(u64, u64)> = records
            .iter()
            .filter(|record| record.duration_ms > 0)
            .map(|record| {
                (
                    record.start_ms,
                    record.start_ms.saturating_add(record.duration_ms),
                )
            })
            .collect();
        spans.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (start, end) in spans {
            match merged.last_mut() {
                Some((_, last_end)) if start <= *last_end => *last_end = (*last_end).max(end),
                _ => merged.push((start, end)),
            }
        }
        let mut segments = Vec::with_capacity(merged.len());
        let mut mapped = 0u64;
        let mut previous_end = 0u64;
        for (start, end) in merged {
            mapped += start.saturating_sub(previous_end).min(IDLE_CAP_MS);
            segments.push((start, end, mapped));
            mapped += end.saturating_sub(start);
            previous_end = end;
        }
        Self {
            segments,
            total: mapped,
        }
    }

    /// The mapped position of an instant on the turn's clock.
    pub(crate) fn map(&self, ms: u64) -> u64 {
        for (start, end, mapped) in &self.segments {
            if ms < *start {
                // Idle before this interval: the wait adds at most the cap.
                let previous_end = self
                    .segments
                    .iter()
                    .take_while(|(s, _, _)| s < start)
                    .map(|(_, e, _)| *e)
                    .max()
                    .unwrap_or(0);
                return *mapped - (start - previous_end).min(IDLE_CAP_MS)
                    + ms.saturating_sub(previous_end).min(IDLE_CAP_MS);
            }
            if ms <= *end {
                return mapped + (ms - start);
            }
        }
        self.total
    }

    pub(crate) fn total(&self) -> u64 {
        self.total
    }
}

/// The bar for one record on the active axis, in eighth-block resolution so a
/// fifty-millisecond call and a three-second call do not both read as one full
/// cell. An instant is a dot, not a full-height rule: a rule per instant stacks
/// into a column the eye reads as a broken border.
pub(crate) fn positioned_bar(
    start_ms: u64,
    dur_ms: u64,
    axis: &ActiveAxis,
    width: usize,
) -> String {
    if width == 0 {
        return String::new();
    }
    if axis.total() == 0 {
        // A turn of instants only: there is no span to scale, so each mark
        // sits at the axis start instead of the axis going blank.
        if dur_ms > 0 {
            return " ".repeat(width);
        }
        let mut s = String::from("·");
        while s.chars().count() < width {
            s.push(' ');
        }
        return s;
    }
    const EIGHTHS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    let total = axis.total();
    let start = axis.map(start_ms);
    let start_col = ((start as f64 / total as f64) * width as f64).round() as usize;
    let start_col = start_col.min(width - 1);
    if dur_ms == 0 {
        let mut s = " ".repeat(start_col);
        s.push('·');
        while s.chars().count() < width {
            s.push(' ');
        }
        return s;
    }
    let end = axis.map(start_ms.saturating_add(dur_ms));
    let span_eighths =
        (((end.saturating_sub(start)) as f64 / total as f64) * width as f64 * 8.0).round() as usize;
    let span_eighths = span_eighths.max(1).min((width - start_col) * 8);
    let full = span_eighths / 8;
    let fraction = span_eighths % 8;
    let mut s = " ".repeat(start_col);
    if full > 0 {
        s.push_str(&"█".repeat(full));
    }
    if fraction > 0 {
        s.push(EIGHTHS[fraction - 1]);
    }
    while s.chars().count() < width {
        s.push(' ');
    }
    s
}

/// One ruler line above the event rows, oriented to the bar area: "0s" left,
/// the axis end right, a dotted axis between. The end is the active span the
/// bars are positioned on, not the turn's wall time. Orients the eye to the time
/// scale so positioned bars read as a real timeline.
pub(crate) fn ruler_line(total_ms: u64, width: usize) -> Line<'static> {
    let left = "0s".to_string();
    // The right label is truncated to what the axis can carry: a span wider
    // than the axis would push the ruler past the bar's columns, and a ruler
    // that is not the axis it labels is worse than a short one.
    let right = truncate_width(
        &format_span_ms(total_ms),
        width.saturating_sub(UnicodeWidthStr::width(left.as_str()) + 1),
    );
    // The axis spans exactly the bar's columns, so a bar's position on the
    // ruler is its position in the turn.
    let mut s = " ".repeat(TIMELINE_PREFIX_W);
    s.push_str(&left);
    let fill = width.saturating_sub(
        UnicodeWidthStr::width(left.as_str()) + UnicodeWidthStr::width(right.as_str()),
    );
    s.push_str(&"·".repeat(fill));
    s.push_str(&right);
    line(vec![sp(s, Color::DarkGray)])
}

#[cfg(test)]
pub(crate) fn bar_width(ms: u64, total: u64, max_w: usize) -> usize {
    if total == 0 {
        0
    } else {
        ((ms as f64 / total as f64) * max_w as f64).round() as usize
    }
}
