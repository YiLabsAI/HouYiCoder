//! V1 brand logo: an arrow piercing the sun (the houyi myth, single shot
//! through the sun). Renders in three sizes: large (welcome), medium (login,
//! distinct), and a tiny mark prepended to the status bar. Brand colors:
//! arrow shaft+head Cyan+BOLD, sun disc Yellow, strike point Red. An ASCII
//! fallback flag (HOUYI_ASCII_FALLBACK=1) swaps unicode box/boxn chars for
//! plain ASCII so restricted terminals stay clean.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

/// True when the terminal should avoid unicode box-drawing / symbol chars.
/// Set via the HOUYI_ASCII_FALLBACK env var (1 or true).
pub fn ascii_fallback() -> bool {
    match std::env::var("HOUYI_ASCII_FALLBACK") {
        Ok(v) => v == "1" || v.eq_ignore_ascii_case("true"),
        Err(_) => false,
    }
}

fn sun_style() -> Style {
    Style::new().fg(Color::Yellow)
}

fn arrow_style() -> Style {
    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
}

fn strike_style() -> Style {
    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
}

fn sun_line(s: &'static str) -> Line<'static> {
    Line::from(vec![Span::styled(s, sun_style())])
}

/// Large logo for the welcome screen. Seven lines, all uniform width 15 so
/// the strike line aligns with the sun disc. The arrow comes from the left
/// margin; the strike point (shaft + spark) lands on the sun's left edge;
/// the sun continues right of the strike. Width is verified by tests.
pub fn large() -> Text<'static> {
    // The art below is plain ASCII; the unicode fallback only swaps the tiny
    // status mark, so large() is the same in both modes.
    let strike = Line::from(vec![
        Span::styled("->", arrow_style()),
        Span::styled("|", strike_style()),
        Span::styled("*", strike_style()),
        Span::styled(":::::::::", sun_style()),
        Span::raw("  "),
    ]);
    large_body(strike)
}

fn large_body(strike: Line<'static>) -> Text<'static> {
    vec![
        sun_line("    ..::..     "),
        sun_line("  .:::::::::.  "),
        sun_line("  :::::::::::  "),
        strike,
        sun_line("  :::::::::::  "),
        sun_line("  ':::::::::'  "),
        sun_line("    '..::..'   "),
    ]
    .into()
}

/// Medium logo for the login screen. Five lines, all uniform width 11, a
/// compact distinct form. Five lines so it fits the login card's logo slot
/// exactly (no overflow into the sign-in line).
pub fn medium() -> Text<'static> {
    let strike = Line::from(vec![
        Span::styled(">", arrow_style()),
        Span::styled("|", strike_style()),
        Span::styled("*", strike_style()),
        Span::styled(":::::::", sun_style()),
        Span::raw(" "),
    ]);
    medium_body(strike)
}

fn medium_body(strike: Line<'static>) -> Text<'static> {
    vec![
        sun_line("  ..::..   "),
        sun_line(" .::::::::."),
        strike,
        sun_line(" '::::::::'"),
        sun_line("  '..::..' "),
    ]
    .into()
}

/// Tiny mark prepended to the status bar: arrow shaft+head then a sun glyph.
pub fn tiny() -> Vec<Span<'static>> {
    if ascii_fallback() {
        return vec![
            Span::styled("->", arrow_style()),
            Span::styled("*", sun_style()),
        ];
    }
    vec![
        Span::styled("->", arrow_style()),
        Span::styled("☉", sun_style()),
    ]
}

/// Wordmark: houyi (Cyan+BOLD) | coder (White). Rendered on its own line
/// below the logo, never inline with the sun disc.
pub fn wordmark() -> Line<'static> {
    vec![
        Span::styled("houyi", arrow_style()),
        Span::styled("coder", Style::new().fg(Color::White)),
    ]
    .into()
}

/// Tagline line.
pub fn tagline() -> Span<'static> {
    Span::styled(
        "shoot down the suns in your code",
        Style::new().fg(Color::DarkGray),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_large_has_seven_lines() {
        let t = large();
        assert_eq!(t.lines.len(), 7);
    }

    #[test]
    fn test_medium_has_five_lines() {
        let t = medium();
        assert_eq!(t.lines.len(), 5);
    }

    #[test]
    fn test_large_lines_uniform_width() {
        // All large() lines must be exactly width 15 or the strike misaligns
        // from the sun disc.
        for (i, l) in large().lines.iter().enumerate() {
            assert_eq!(
                l.width(),
                15,
                "large line {i} width {} != 15 -> misaligned",
                l.width()
            );
        }
    }

    #[test]
    fn test_medium_lines_uniform_width() {
        for (i, l) in medium().lines.iter().enumerate() {
            assert_eq!(
                l.width(),
                11,
                "medium line {i} width {} != 11 -> misaligned",
                l.width()
            );
        }
    }

    #[test]
    fn test_large_strike_hits_left() {
        // The strike line is line 3. Columns 0-1 are the arrow, col 2 is the
        // strike shaft, col 3 the spark, cols 4-12 the sun, cols 13-14 blank.
        let strike = &large().lines[3];
        let s: String = strike.spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert!(
            s.starts_with("->|*"),
            "strike line must start with the arrow + strike point, got [{s}]"
        );
        assert_eq!(s.len(), 15);
    }

    #[test]
    fn test_wordmark_two_spans() {
        assert_eq!(wordmark().spans.len(), 2);
    }

    #[test]
    fn test_tiny_two_spans() {
        assert_eq!(tiny().len(), 2);
    }
}
