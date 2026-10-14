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
