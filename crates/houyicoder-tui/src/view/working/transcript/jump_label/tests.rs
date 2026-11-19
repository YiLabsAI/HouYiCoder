//! Tests for the jump-to-bottom badge text and border placement.

use ratatui::layout::Rect;

use crate::scroll::NewMessageCount;

use super::{border_column, format_label};

#[test]
fn test_border_column_centers() {
    let area = Rect::new(0, 20, 80, 3);
    assert_eq!(border_column(area, 16), 32);
    let narrow = Rect::new(0, 20, 10, 3);
    assert_eq!(border_column(narrow, 10), 0);
}

#[test]
fn test_label_text_states() {
    assert_eq!(
        format_label(NewMessageCount {
            count: 0,
            is_lower_bound: false,
        }),
        " Jump to bottom "
    );
    assert_eq!(
        format_label(NewMessageCount {
            count: 1,
            is_lower_bound: false,
        }),
        " 1 new message "
    );
    assert_eq!(
        format_label(NewMessageCount {
            count: 3,
            is_lower_bound: false,
        }),
        " 3 new messages "
    );
    assert_eq!(
        format_label(NewMessageCount {
            count: 100,
            is_lower_bound: false,
        }),
        " 99+ new messages "
    );
}
