//! Transient toast: one line, right-aligned, overlaid on the transcript's
//! bottom row (so it takes no layout height — the input box does not shift
//! when a toast appears or vanishes). Only the current notification renders;
//! queued ones wait. Default color is a friendly cyan (not dim gray — gray is
//! low-contrast and easy to miss); copy toasts override to green (success).

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Style},
    text::Span,
    widgets::Paragraph,
};

use crate::notifications::{NotifKind, Notification};
use crate::state::App;

const FRIENDLY_DEFAULT: Color = Color::Cyan;

/// Overlay the toast on the given transcript area's bottom row. Drawn after
/// the transcript so it paints over the row it covers. No-op when nothing is
/// current.
pub fn draw_toast(f: &mut Frame, transcript_area: Rect, app: &App) {
    let Some(n) = app.notifications.current() else {
        return;
    };
    let area = Rect {
        x: transcript_area.x,
        y: transcript_area.bottom().saturating_sub(1),
        width: transcript_area.width,
        height: 1,
    };
    if area.width == 0 {
        return;
    }
    let (text, color) = text_and_color(n);
    let span = Span::styled(text, Style::new().fg(color));
    f.render_widget(Paragraph::new(span).alignment(Alignment::Right), area);
}

fn text_and_color(n: &Notification) -> (String, Color) {
    match &n.kind {
        NotifKind::Text { text, color } => (text.clone(), color.unwrap_or(FRIENDLY_DEFAULT)),
    }
}
