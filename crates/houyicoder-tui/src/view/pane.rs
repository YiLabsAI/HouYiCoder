//! The Pane primitive: a region bounded by a colored top line (a Divider)
//! and horizontal padding, rendered inline below the transcript tail. A
//! design-system Pane so every slash-command management
//! surface (/permissions /sandbox /help /status /model) reuses one style
//! instead of each inventing a rounded-border card or a full-screen overlay.
//! Not a floating modal, not a full-screen takeover — a full-width ─ line
//! plus the cleared region beneath it.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

/// The horizontal padding applied to the region below the Divider (paddingX
/// of 2).
const PAD_X: u16 = 2;

/// Render the Pane frame into area and draw content into the padded region
/// below the Divider. Row 0 of area is a full-width themed ─; rows 1.. are
/// cleared (so the transcript tail above does not read through) and the content
/// draws into a horizontally-padded inner rect. When area is too short or
/// narrow to frame, clears the area and delegates to content with the full
/// area (best effort, never panics).
pub fn render(f: &mut Frame, area: Rect, color: Color, content: impl FnOnce(&mut Frame, Rect)) {
    if area.height < 2 || area.width < 2 * PAD_X + 1 {
        f.render_widget(Clear, area);
        content(f, area);
        return;
    }
    // The Divider: a full-width themed ─ at the top row of the Pane.
    let divider = Line::from(Span::styled(
        "─".repeat(area.width as usize),
        Style::new().fg(color).add_modifier(Modifier::BOLD),
    ));
    f.render_widget(
        Paragraph::new(divider),
        Rect {
            y: area.y,
            height: 1,
            x: area.x,
            width: area.width,
        },
    );
    // Clear the region below the Divider so content reads on a blank slate,
    // not over the transcript tail. Then hand the padded inner rect to content.
    let body = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        x: area.x,
        width: area.width,
    };
    f.render_widget(Clear, body);
    let inner = Rect {
        y: body.y,
        height: body.height,
        x: body.x.saturating_add(PAD_X),
        width: body.width.saturating_sub(2 * PAD_X),
    };
    content(f, inner);
}

#[cfg(test)]
mod tests;
