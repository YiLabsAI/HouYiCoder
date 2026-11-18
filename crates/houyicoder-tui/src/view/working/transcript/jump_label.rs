//! The jump-to-bottom overlay: a bright label on the transcript's bottom
//! row while the reader is scrolled back, naming how far the view is behind
//! and the key that returns to the tail. Its hit rectangle is the label
//! span alone, so a click beside it reaches the transcript surface.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::state::{App, Pane, ViewportMode};

use super::MESSAGE_COUNT_CAP;

/// Draw the overlay over the transcript's last row, or clear its hit
/// rectangle when it is not shown.
pub(super) fn draw(f: &mut Frame, inner: Rect, app: &App) {
    // Hidden while following the tail, with a search or queue overlay open,
    // or in Scroll mode, whose status bar already advertises Esc=tail.
    let label_visible = app.ui.navigation.viewport != ViewportMode::Scroll
        && app.ui.navigation.pane == Pane::Transcript
        && !app.active_scroll().is_following_tail()
        && !app.ui.transcript_view.search_active();
    if label_visible {
        let new = app.new_message_count();
        let label = if new.count > 0 {
            let shown = new.count.min(MESSAGE_COUNT_CAP);
            let floor = if new.count > MESSAGE_COUNT_CAP || new.is_lower_bound {
                "+"
            } else {
                ""
            };
            format!(" {shown}{floor} new | End ")
        } else {
            " Bottom | End ".to_string()
        };
        let row = inner.y.saturating_add(inner.height.saturating_sub(1));
        let label_w = UnicodeWidthStr::width(label.as_str()).min(inner.width as usize) as u16;
        // The label sits at the transcript's true center. It yields to the
        // left margin only when the row under it already shows a fold chord
        // inside the span it would cover: the overlay carries Clear, so
        // covering that chord would hide the only text naming the key, and
        // the label's hit rectangle would take the click the chord answers.
        let centered = inner.x + inner.width.saturating_sub(label_w) / 2;
        let covered = app
            .ui
            .frame
            .layout
            .last_transcript_rows
            .borrow()
            .get(usize::from(inner.height.saturating_sub(1)))
            .map(|(_, text)| covered_span(text, centered, inner, label_w))
            .unwrap_or_default();
        let x = label_column(inner, label_w, &covered);
        let rect = Rect::new(x, row, label_w, 1);
        f.render_widget(Clear, rect);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                label,
                Style::new().fg(Color::White).bg(Color::Indexed(238)),
            ))),
            rect,
        );
        app.ui.frame.layout.jump_to_bottom_rect.set(rect);
    } else {
        app.ui
            .frame
            .layout
            .jump_to_bottom_rect
            .set(Rect::new(0, 0, 0, 0));
    }
}

/// The words a fold row carries when its hint is the only text naming the
/// chord that reveals the folded rows.
const FOLD_CHORD_TEXT: &str = "ctrl+o to";

/// The label's column: the transcript's true center, or the left margin
/// when the span it would cover already names a fold chord.
fn label_column(inner: Rect, label_w: u16, covered: &str) -> u16 {
    let centered = inner.x + inner.width.saturating_sub(label_w) / 2;
    if covered.contains(FOLD_CHORD_TEXT) {
        (inner.x + 2).min(inner.x + inner.width.saturating_sub(label_w))
    } else {
        centered
    }
}

/// The row text a label at this column and width would hide, taken by
/// display columns so a wide character counts as the two it occupies.
fn covered_span(row: &str, x: u16, inner: Rect, width: u16) -> String {
    let start = usize::from(x.saturating_sub(inner.x));
    let end = start.saturating_add(usize::from(width));
    let mut text = String::new();
    let mut column = 0usize;
    for ch in row.chars() {
        let next = column + UnicodeWidthChar::width(ch).unwrap_or(0);
        if column >= start && next <= end {
            text.push(ch);
        }
        column = next;
        if column >= end {
            break;
        }
    }
    text
}

#[cfg(test)]
mod tests;
