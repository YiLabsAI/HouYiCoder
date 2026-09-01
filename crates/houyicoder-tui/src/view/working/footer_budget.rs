//! Row budget for the pinned footer strips.
//!
//! Each strip used to size itself, so the transcript floor was enforced
//! against one strip at a time while their sum could still starve the
//! conversation. Allocation happens once, for all of them, in priority order.

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
    std::cmp::max(10, total_h / 2)
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
mod tests {
    use super::*;

    #[test]
    fn test_roomy_window_grants_wants() {
        let rows = allocate(50, 3, 3, 3);
        assert_eq!(rows, FooterRows { queue: 3, fleet: 3 });
    }

    /// A squeezed window pays existence out first: both strips collapse to
    /// their summary line rather than one keeping detail while the other
    /// disappears.
    #[test]
    fn test_squeeze_collapses_both() {
        assert_eq!(allocate(24, 9, 3, 3), FooterRows { queue: 1, fleet: 1 });
    }

    /// With detail room for one strip only, priority decides: queued input is
    /// the user's own unsent content and nothing else shows it.
    #[test]
    fn test_queue_gets_detail() {
        assert_eq!(allocate(24, 7, 3, 3), FooterRows { queue: 3, fleet: 1 });
    }

    /// With the budget gone the queue still keeps one row -- losing sight of
    /// unsent input costs the user an action -- and the fleet drops out, since
    /// the status bar counts it and the pane lists it.
    #[test]
    fn test_exhausted_keeps_queue() {
        let rows = allocate(24, 12, 3, 3);
        assert_eq!(rows, FooterRows { queue: 1, fleet: 0 });
    }

    #[test]
    fn test_small_window_summaries_only() {
        assert_eq!(allocate(18, 3, 3, 3), FooterRows { queue: 1, fleet: 1 });
    }

    #[test]
    fn test_empty_strips_take_nothing() {
        assert_eq!(allocate(50, 3, 0, 0), FooterRows { queue: 0, fleet: 0 });
    }

    /// The invariant the per-strip sizing could not hold: across window sizes
    /// and input heights, the rows the footer takes never push the transcript
    /// below its floor -- unless the floor itself does not fit, where the
    /// queue's guaranteed row is the deliberate exception.
    #[test]
    fn test_floor_survives_both_strips() {
        for total in 20u16..60 {
            for input in 1u16..12 {
                let rows = allocate(total, input, 3, 3);
                let taken = input + 1 + rows.queue + rows.fleet;
                let left = total.saturating_sub(taken);
                let floor = transcript_floor(total);
                let fits = total.saturating_sub(input + 1) > floor;
                assert!(
                    left >= floor || !fits,
                    "total {total} input {input} left {left} floor {floor} rows {rows:?}"
                );
            }
        }
    }
}
