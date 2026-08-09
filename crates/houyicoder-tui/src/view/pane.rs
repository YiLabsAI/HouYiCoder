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
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn draw_pane(width: u16, height: u16, color: Color) -> TestBackend {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                let area = Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                };
                render(f, area, color, |f, inner| {
                    f.render_widget(
                        Paragraph::new("body").style(Style::new().fg(Color::White)),
                        inner,
                    );
                });
            })
            .expect("draw");
        terminal.backend().clone()
    }

    /// The Divider is a full-width ─ line on the top row in the theme color.
    #[test]
    fn test_divider_fills_top_row() {
        let backend = draw_pane(20, 6, Color::Cyan);
        let buf = backend.buffer();
        // Top row is all ─.
        for x in 0..20 {
            let cell = &buf[(x, 0)];
            assert_eq!(cell.symbol(), "─", "row 0 col {x} should be ─");
            assert_eq!(cell.fg, Color::Cyan, "row 0 col {x} should be Cyan");
        }
    }

    /// The content draws into the padded region below the Divider, not over it.
    #[test]
    fn test_content_draws_below_divider() {
        let backend = draw_pane(20, 6, Color::Cyan);
        let buf = backend.buffer();
        // Row 1, col 2 is the start of "body" (padding 2). Row 0 is the Divider.
        assert_eq!(buf[(2, 1)].symbol(), "b");
        assert_eq!(buf[(3, 1)].symbol(), "o");
        assert_eq!(buf[(4, 1)].symbol(), "d");
        assert_eq!(buf[(5, 1)].symbol(), "y");
        // Row 0 col 2 is the Divider, not 'b'.
        assert_eq!(buf[(2, 0)].symbol(), "─");
    }

    /// The region below the Divider is cleared before content draws (no stale
    /// background bleed); a tall pane shows blank rows beyond the content.
    #[test]
    fn test_region_below_is_cleared() {
        let backend = draw_pane(12, 5, Color::Cyan);
        let buf = backend.buffer();
        // Row 2 (below the "body" on row 1) is blank in the padded region.
        assert_eq!(buf[(2, 2)].symbol(), " ");
    }

    /// A too-short area degrades gracefully: clears and hands the full area to
    /// content without the Divider (never panics).
    #[test]
    fn test_area_degrades_no_panic() {
        let backend = draw_pane(5, 1, Color::Cyan);
        let buf = backend.buffer();
        // Height 1 is too short to frame; the content gets the full row, no ─.
        assert_eq!(buf[(0, 0)].symbol(), "b");
    }
}
