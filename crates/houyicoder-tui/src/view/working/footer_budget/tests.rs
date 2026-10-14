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
