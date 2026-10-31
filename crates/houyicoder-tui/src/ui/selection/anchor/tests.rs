//! Row-identity resolution: which row each end lands on after the set above
//! it gains, loses, or shortens a line.

use super::{point_at, row_of_point};
use crate::transcript::row_id::RowId;
use crate::ui::selection::{Selection, Span, SpanKind};

/// Names for rows: an empty entry is a row with no identity (the live tail),
/// anything else is the line the row belongs to.
fn ids(rows: &[&str]) -> Vec<Option<RowId>> {
    rows.iter()
        .map(|r| (!r.is_empty()).then(|| RowId::from(*r)))
        .collect()
}

#[test]
fn test_point_round_trips() {
    let rows = ids(&["a", "a", "a", "b"]);
    for row in 0..rows.len() {
        let p = point_at(&rows, row).expect("named row");
        assert_eq!(row_of_point(&rows, Some(&p), row), row);
    }
}

#[test]
fn test_point_follows_added_rows() {
    let before = ids(&["a", "a", "b"]);
    let p = point_at(&before, 2).expect("named row");
    let after = ids(&["x", "a", "a", "b"]);
    assert_eq!(row_of_point(&after, Some(&p), 2), 3);
}

#[test]
fn test_point_clamps_shrunk_line() {
    let before = ids(&["a", "a", "a"]);
    let p = point_at(&before, 2).expect("named row");
    // The line still holds rows, but fewer than the point stood past: the
    // end lands on the line's last row instead of drifting into the next.
    assert_eq!(row_of_point(&ids(&["a"]), Some(&p), 2), 0);
    assert_eq!(row_of_point(&ids(&["a", "a"]), Some(&p), 2), 1);
}

#[test]
fn test_point_survives_gone_line() {
    let before = ids(&["a", "b"]);
    let p = point_at(&before, 1).expect("named row");
    // The line left the set; the recorded row is all that remains, and it
    // may now hold other text. This is the documented fallback.
    assert_eq!(row_of_point(&ids(&["a", "c"]), Some(&p), 1), 1);
}

#[test]
fn test_unnamed_row_keeps_row() {
    let rows = ids(&["a", "", "b"]);
    assert!(point_at(&rows, 1).is_none());
    assert_eq!(row_of_point(&rows, None, 1), 1);
}

#[test]
fn test_repeat_name_takes_first() {
    // A line is named once per set, so a repeat means a name was reused; the
    // resolver takes the first run rather than guessing between them.
    let rows = ids(&["a", "b", "a"]);
    let p = point_at(&rows, 2).expect("named row");
    assert_eq!(row_of_point(&rows, Some(&p), 2), 0);
}

#[test]
fn test_bind_resolves_shift() {
    let before = ids(&["a", "a", "b"]);
    let mut sel = Selection {
        anchor: Some((0, 0)),
        focus: Some((4, 2)),
        span_origin: Some(Span {
            lo: (0, 2),
            hi: (4, 2),
            kind: SpanKind::Word,
        }),
        ..Default::default()
    };
    sel.bind(&before);
    let mut after_ids = ids(&["x"]);
    after_ids.extend(before.clone());
    let moved = sel.resolved(&after_ids);
    assert_eq!(moved.anchor.map(|(_, row)| row), Some(1));
    assert_eq!(moved.focus.map(|(_, row)| row), Some(3));
    // The multi-click origin moves with its line, so a drag that extends
    // the span afterwards still grows from the picked word.
    assert_eq!(moved.span_origin.map(|s| s.lo.1), Some(3));
    assert_eq!(moved.span_origin.map(|s| s.hi.1), Some(3));
}
