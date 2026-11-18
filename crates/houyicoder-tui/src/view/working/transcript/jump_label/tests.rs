//! Tests for the jump-to-bottom label's placement rules.

use ratatui::layout::Rect;

use super::{covered_span, label_column};

/// An ordinary scrollback centers the label; nothing has to be dodged.
#[test]
fn test_label_column_centers() {
    let inner = Rect::new(0, 0, 80, 20);
    assert_eq!(label_column(inner, 14, "history line 49"), 33);
    assert_eq!(label_column(inner, 14, ""), 33);
}

/// A fold chord inside the span the label would cover moves it to the left
/// margin, so the only text naming the key stays readable and clickable.
#[test]
fn test_label_column_yields() {
    let inner = Rect::new(0, 0, 80, 20);
    assert_eq!(
        label_column(inner, 14, " (ctrl+o to exp"),
        2,
        "a covered chord moves the label to the margin"
    );
}

/// The margin yields when the transcript is narrower than the margin plus
/// the label, so the label never starts past the right edge.
#[test]
fn test_label_column_stays_inside() {
    let inner = Rect::new(0, 0, 10, 20);
    assert_eq!(label_column(inner, 10, "(ctrl+o to expand)"), 0);
}

/// A wide character occupies two columns, so the span a label covers is
/// measured in display columns and aligns with what the reader sees.
#[test]
fn test_covered_span_counts_width() {
    let inner = Rect::new(0, 0, 80, 20);
    assert_eq!(covered_span("中文abc", 2, inner, 4), "文ab");
}
