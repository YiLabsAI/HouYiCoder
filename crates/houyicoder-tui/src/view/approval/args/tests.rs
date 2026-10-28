//! The arguments block's row budget and fold: what it shows when the call's
//! arguments outgrow the room the card can give them.

use ratatui::text::Line;

use super::{MAX_APPROVAL_ARGS_ROWS, fold_rows, row_cap};

/// The card width the fold tests wrap to; wide enough that the marker keeps
/// its key offer.
const WIDTH: u16 = 80;

/// A block taller than its budget keeps the first and the last rows and names
/// how many it released between them.
#[test]
fn test_fold_keeps_head_tail() {
    let rows: Vec<Line<'static>> = (0..40).map(|i| Line::from(format!("row {i}"))).collect();
    let folded = fold_rows(rows, 12, false, WIDTH);
    assert_eq!(folded.len(), 12);
    assert_eq!(folded[0].to_string(), "row 0");
    assert_eq!(folded[11].to_string(), "row 39");
    assert!(folded[5].to_string().contains("29 lines hidden"));
    assert!(
        !folded.iter().any(|row| row.to_string().contains("row 20")),
        "the middle is what the fold releases"
    );
}

/// The smallest budget that still shows a row keeps the first one beside the
/// marker, so the user reads what the call starts with rather than only where
/// it ends.
#[test]
fn test_fold_keeps_first_row() {
    let rows: Vec<Line<'static>> = (0..5).map(|i| Line::from(format!("row {i}"))).collect();
    let folded = fold_rows(rows.clone(), 2, false, WIDTH);
    assert_eq!(folded.len(), 2);
    assert_eq!(folded[0].to_string(), "row 0");
    assert!(folded[1].to_string().contains("4 lines hidden"));
}

/// A budget of one row can hold the marker only, which still tells the user
/// how much the card did not show.
#[test]
fn test_fold_single_row_marker() {
    let rows: Vec<Line<'static>> = (0..5).map(|i| Line::from(format!("row {i}"))).collect();
    let folded = fold_rows(rows, 1, false, WIDTH);
    assert_eq!(folded.len(), 1);
    assert!(folded[0].to_string().contains("5 lines hidden"));
}

/// A block that fits its budget is drawn whole.
#[test]
fn test_fold_keeps_fitting_rows() {
    let rows: Vec<Line<'static>> = (0..4).map(|i| Line::from(format!("row {i}"))).collect();
    assert_eq!(fold_rows(rows.clone(), 12, false, WIDTH), rows);
    assert_eq!(fold_rows(rows, 4, false, WIDTH).len(), 4);
}

/// The marker row carries the key that moves the released rows in or out,
/// naming the direction the next press takes.
#[test]
fn test_fold_marker_offers_key() {
    let rows: Vec<Line<'static>> = (0..40).map(|i| Line::from(format!("row {i}"))).collect();
    let marker = fold_rows(rows.clone(), 12, false, WIDTH)[5].to_string();
    assert!(marker.contains("ctrl+o to expand"), "{marker}");
    let expanded = fold_rows(rows, 12, true, WIDTH)[5].to_string();
    assert!(expanded.contains("29 lines hidden"), "{expanded}");
    assert!(expanded.contains("ctrl+o to collapse"), "{expanded}");
}

/// The cap is the fixed maximum on a tall terminal and a third of its height
/// on a short one, never zero.
#[test]
fn test_args_budget_bounds_rows() {
    assert_eq!(row_cap(120), MAX_APPROVAL_ARGS_ROWS);
    assert_eq!(row_cap(30), 10);
    assert_eq!(row_cap(6), 2);
    assert_eq!(row_cap(2), 1);
}
