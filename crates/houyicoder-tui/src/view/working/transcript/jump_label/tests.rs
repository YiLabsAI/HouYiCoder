//! Tests for the jump-to-bottom badge text, highlight style, and border placement.

use houyicoder_platform::desktop::glyphs::GlyphSet;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use unicode_width::UnicodeWidthStr;

use crate::scroll::NewMessageCount;

use super::{badge_style, border_column, format_label};

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
        format_label(
            NewMessageCount {
                count: 0,
                is_lower_bound: false,
            },
            "▾",
            80,
        ),
        " Jump to bottom (click) ▾ "
    );
    assert_eq!(
        format_label(
            NewMessageCount {
                count: 1,
                is_lower_bound: false,
            },
            "▾",
            80,
        ),
        " 1 new message (click) ▾ "
    );
    assert_eq!(
        format_label(
            NewMessageCount {
                count: 3,
                is_lower_bound: false,
            },
            "▾",
            80,
        ),
        " 3 new messages (click) ▾ "
    );
    assert_eq!(
        format_label(
            NewMessageCount {
                count: 100,
                is_lower_bound: false,
            },
            "▾",
            80,
        ),
        " 99+ new messages (click) ▾ "
    );
}

#[test]
fn test_label_compact_narrow_width() {
    assert_eq!(
        format_label(
            NewMessageCount {
                count: 0,
                is_lower_bound: false,
            },
            "▾",
            24,
        ),
        " Jump to bottom ▾ "
    );
    assert_eq!(
        format_label(
            NewMessageCount {
                count: 3,
                is_lower_bound: false,
            },
            "v",
            24,
        ),
        " 3 new messages v "
    );
}

#[test]
fn test_label_neutral_cjk_width() {
    for set in [GlyphSet::Safe, GlyphSet::Console, GlyphSet::Rich] {
        for count in [0, 1, 100] {
            let text = format_label(
                NewMessageCount {
                    count,
                    is_lower_bound: false,
                },
                set.down_mark(),
                80,
            );
            assert_eq!(
                UnicodeWidthStr::width(text.as_str()),
                UnicodeWidthStr::width_cjk(text.as_str()),
                "badge width must stay identical under CJK rules in {set:?}: {text:?}"
            );
        }
    }
}

#[test]
fn test_badge_highlight_style() {
    let rest = badge_style(false);
    let active = badge_style(true);
    assert_eq!(rest.bg, Some(Color::Indexed(238)));
    assert_eq!(active.bg, Some(Color::Indexed(240)));
    assert!(active.add_modifier.contains(Modifier::BOLD));
    assert!(!rest.add_modifier.contains(Modifier::BOLD));
}
