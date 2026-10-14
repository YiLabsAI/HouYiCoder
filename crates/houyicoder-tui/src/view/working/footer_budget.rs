//! Row budget for the pinned footer strips.
//!
//! Each strip used to size itself, so the transcript floor was enforced
//! against one strip at a time while their sum could still starve the
//! conversation. Allocation happens once, for all of them, in priority order.

use std::cmp::max;

/// Rows granted to each pinned strip. A strip given one row renders its
/// one-line summary; zero means it is not drawn at all.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct FooterRows {
    pub queue: u16,
    pub fleet: u16,
}

/// Rows the transcript keeps whatever the footer wants: ten rows, or half the
/// window on anything taller than twenty.
fn transcript_floor(total_h: u16) -> u16 {
    max(10, total_h / 2)
}

/// Split the rows left over from the input box and the status bar between the
/// strips that want them.
///
/// Queued input outranks agent progress: it is the user's own unsent content,
/// and nothing else displays it, while a delegation's progress is also in the
/// transcript and in the agents pane. So the queue keeps a row even when the
/// budget runs out, and the fleet is the one that can reach zero.
pub(super) fn allocate(total_h: u16, input_h: u16, queue_want: u16, fleet_want: u16) -> FooterRows {
    if queue_want == 0 && fleet_want == 0 {
        return FooterRows { queue: 0, fleet: 0 };
    }
    // Under twenty rows the floor would claim everything, so each strip that
    // wants rows gets exactly its summary line.
    if total_h < 20 {
        return FooterRows {
            queue: queue_want.min(1),
            fleet: fleet_want.min(1),
        };
    }
    let budget = total_h
        .saturating_sub(input_h)
        .saturating_sub(1)
        .saturating_sub(transcript_floor(total_h));
    // Existence before detail: a strip that wants rows gets its summary line
    // first, so priority decides who gets detail rather than who gets seen at
    // all. Two summaries tell the user more than one strip's full rows and
    // silence from the other.
    let mut left = budget;
    let mut queue = 0;
    let mut fleet = 0;
    if queue_want > 0 {
        queue = 1;
        left = left.saturating_sub(1);
    }
    if fleet_want > 0 && left > 0 {
        fleet = 1;
        left -= 1;
    }
    let more = (queue_want - queue).min(left);
    queue += more;
    left -= more;
    fleet += (fleet_want.saturating_sub(fleet)).min(left);
    FooterRows { queue, fleet }
}

#[cfg(test)]
mod tests;
