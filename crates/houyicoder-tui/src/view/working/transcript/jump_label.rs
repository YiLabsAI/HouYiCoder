//! The jump-to-bottom badge: centered on the input box's top border rule while
//! the reader is scrolled back, naming how many new replies have landed and
//! advertising the click and downward jump action. Riding on the border rule
//! keeps every transcript row and inline fold hint uncovered and avoids
//! splitting wide characters or user-row background bands.

use houyicoder_platform::desktop::glyphs::detect_glyph_set;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::scroll::NewMessageCount;
use crate::state::{App, Pane, ViewportMode};

use super::MESSAGE_COUNT_CAP;

/// Draw the jump-to-bottom badge centered on the top border of the input box,
/// or clear its hit rectangle when following the tail.
pub(in super::super) fn draw(f: &mut Frame, input_area: Rect, app: &App) {
    let label_visible = input_area.height > 0
        && input_area.width > 0
        && app.ui.navigation.viewport != ViewportMode::Scroll
        && app.ui.navigation.pane == Pane::Transcript
        && !app.active_scroll().is_following_tail()
        && !app.ui.transcript_view.search_active();
    if label_visible {
        let down_mark = detect_glyph_set().down_mark();
        let label = format_label(app.new_message_count(), down_mark, input_area.width);
        let label_w = UnicodeWidthStr::width(label.as_str()).min(input_area.width as usize) as u16;
        let x = border_column(input_area, label_w);
        let rect = Rect::new(x, input_area.y, label_w, 1);
        let style = badge_style(app.active_scroll().jump_highlighted());
        f.render_widget(Paragraph::new(Line::from(Span::styled(label, style))), rect);
        app.ui.frame.layout.jump_to_bottom_rect.set(rect);
    } else {
        app.ui
            .frame
            .layout
            .jump_to_bottom_rect
            .set(Rect::new(0, 0, 0, 0));
    }
}

fn badge_style(highlighted: bool) -> Style {
    if highlighted {
        Style::new()
            .fg(Color::White)
            .bg(Color::Indexed(240))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::White).bg(Color::Indexed(238))
    }
}

fn format_label(new: NewMessageCount, down_mark: &str, max_width: u16) -> String {
    let core = if new.count == 0 {
        "Jump to bottom".to_string()
    } else {
        let shown = new.count.min(MESSAGE_COUNT_CAP);
        let floor = if new.count > MESSAGE_COUNT_CAP || new.is_lower_bound {
            "+"
        } else {
            ""
        };
        let noun = if shown == 1 && floor.is_empty() {
            "message"
        } else {
            "messages"
        };
        format!("{shown}{floor} new {noun}")
    };
    let full = format!(" {core} (click) {down_mark} ");
    if UnicodeWidthStr::width(full.as_str()) + 2 <= usize::from(max_width) {
        full
    } else {
        format!(" {core} {down_mark} ")
    }
}

fn border_column(area: Rect, label_w: u16) -> u16 {
    area.x + area.width.saturating_sub(label_w) / 2
}

#[cfg(test)]
mod tests;
