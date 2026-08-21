//! Slash-command palette popover. Bottom-anchored (drops down from the input
//! area), small (top-N visible with scroll), and filtered live by the inline
//! query the user types. Each row is the command name plus a one-line help
//! truncated to fit the popover width; the selected row is inverted. Arrow
//! keys navigate the filtered list (handled in keys.rs).

use houyicoder_protocol::frontend::SlashCommand;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph},
};

use crate::state::App;
use crate::view::line_wrap::truncate_width;

/// Maximum rows of commands the popover shows before scrolling. Keeps the
/// popover small so it never covers the whole working surface.
const MAX_VISIBLE: usize = 8;

/// Maximum popover width in columns. A small bottom-anchored box, never
/// full-screen. Long command help is truncated to fit this width.
const MAX_WIDTH: u16 = 60;

/// Name column width (commands are padded to this so help aligns). The slash
/// name plus padding; the longest name (/release-notes = 14) fits.
const NAME_COL: usize = 16;

/// Render the palette as a bottom-anchored popover in the given area. The
/// area should sit just above the input box (see view::palette_area).
pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Cyan))
        .title(format!(
            " / commands | filter: {} | Up/Down=move Enter=run Esc=close ",
            app.palette.query,
        ));
    f.render_widget(block, area);

    // Use the bordered block's inner() so the list never overdraws the border
    // or the title. A NONE-borders block would return the full area and let
    // the first/last list rows clobber the title and bottom border.
    let inner = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .inner(area);
    let filtered = app.palette_filtered();
    let help_max = help_max_width(inner.width);
    let items: Vec<ListItem> = filtered
        .iter()
        .map(|c| ListItem::new(format_one(*c, help_max)))
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.palette.sel.min(filtered.len().saturating_sub(1))));
    let list = List::new(items)
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    f.render_stateful_widget(list, inner, &mut state);

    // If the filtered list is empty, either the user is typing an argument
    // after a selected arg-taking command (show the arg hint — the
    // "hint-after-space" behavior) or genuinely nothing matches (say so).
    if filtered.is_empty() {
        let hint = arg_hint_for(&app.palette.query);
        let text = match hint {
            Some(hint) => format!("  {} {}  ", arg_command_name(&app.palette.query), hint),
            None => "  no command matches your filter".to_string(),
        };
        f.render_widget(
            Paragraph::new(text).style(Style::new().fg(Color::DarkGray)),
            inner,
        );
    }
}

/// The argument usage hint for the command the user is typing an argument to
/// (the query is a command name + a trailing space, e.g. "resume "). None when
/// the command takes no argument — the palette then shows the plain "no match"
/// row. The hint shows for any arg-capable command, not just takes_arg ones
/// (/resume /export /debug take an optional arg), so the spaced form is guided.
fn arg_hint_for(query: &str) -> Option<&'static str> {
    let name = arg_command_name(query);
    let cmd = SlashCommand::parse(&format!("/{name}"))?;
    let hint = cmd.arg_hint();
    if hint.is_empty() { None } else { Some(hint) }
}

/// The command name a query begins with (everything before the first space),
/// so "resume file.json" resolves to the /resume command. Empty when the query
/// has no leading command token.
fn arg_command_name(query: &str) -> &str {
    query.split_whitespace().next().unwrap_or("")
}

/// Available width for the help text on one row. The list widget reserves
/// NAME_COL for the padded name, 2 spaces of separator, and 2 columns for the
/// highlight symbol ("> ") on the selected row. Anything left is help.
fn help_max_width(inner_width: u16) -> usize {
    let reserved = NAME_COL + 2 + 2; // name + separator + highlight symbol
    (inner_width as usize).saturating_sub(reserved)
}

/// One row: padded name then a dim one-line help truncated to fit the
/// popover width (with an ellipsis when cut), so the eye can scan names and
/// read help only when needed without overflow into the border.
fn format_one(cmd: SlashCommand, help_max: usize) -> Line<'static> {
    let name = pad_name(cmd.name());
    Line::from(vec![
        Span::styled(
            name,
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            truncate_width(cmd.help(), help_max),
            Style::new().fg(Color::DarkGray),
        ),
    ])
}

/// Pad a command name to a fixed column so help text aligns across rows.
fn pad_name(name: &str) -> String {
    if name.len() >= NAME_COL {
        name.to_string()
    } else {
        format!("{name:<NAME_COL$}")
    }
}

/// The bottom-anchored popover rect: a small box that drops down from just
/// above the input row, never full-screen. Width is capped at MAX_WIDTH (with
/// a small side margin) so the popover reads as a small overlay, not a wall;
/// height is capped so at most MAX_VISIBLE rows + border fit.
pub fn area(screen: Rect) -> Rect {
    let visible = MAX_VISIBLE.min(screen.height as usize / 2);
    let height = (visible + 2) as u16; // +2 for the rounded border
    let bottom_gap = 4; // leave room for the input box + status bar
    let top = screen.height.saturating_sub(height + bottom_gap);
    let width = screen.width.min(MAX_WIDTH);
    let x = screen.x + (screen.width.saturating_sub(width)) / 2;
    Rect::new(x, screen.y + top, width, height.min(screen.height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pad_name_aligns() {
        // short names pad to NAME_COL so help columns align
        assert_eq!(pad_name("/a"), format!("/a{}", " ".repeat(NAME_COL - 2)));
        // names still under NAME_COL also pad to NAME_COL
        assert_eq!(pad_name("/release-notes"), format!("/release-notes  "));
    }

    #[test]
    fn test_area_bottom_anchored() {
        let screen = Rect::new(0, 0, 80, 24);
        let a = area(screen);
        assert!(a.height <= 12, "popover must stay small, got {}", a.height);
        assert!(
            a.y + a.height <= screen.y + screen.height,
            "popover must not overflow the screen"
        );
        assert!(
            a.width <= MAX_WIDTH,
            "popover width must be capped, got {}",
            a.width
        );
        // centered: equal margin both sides
        assert_eq!(
            a.x - screen.x,
            (screen.width - a.width) / 2,
            "popover should be horizontally centered"
        );
    }

    #[test]
    fn test_format_one_name_help() {
        let line = format_one(SlashCommand::Spec, 40);
        assert_eq!(line.spans.len(), 3);
    }

    #[test]
    fn test_truncate_help_keeps_short() {
        assert_eq!(truncate_width("short", 10), "short");
    }

    #[test]
    fn test_truncate_help_adds_ellipsis() {
        let out = truncate_width("a very long help string", 10);
        assert!(out.ends_with('\u{2026}'));
        assert!(
            unicode_width::UnicodeWidthStr::width(out.as_str()) <= 10,
            "width overflow: {out}"
        );
    }

    #[test]
    fn test_truncate_help_never_exceeds() {
        // The bug we are guarding against: help must not overflow the popover
        // border. Any help, any max, the result width must be <= max.
        for (help, max) in [
            ("short", 10),
            ("a very long help string", 10),
            ("a very long help string", 38),
            ("a very long help string", 3),
            ("a very long help string", 2),
            ("a very long help string", 0),
        ] {
            let out = truncate_width(help, max);
            assert!(
                unicode_width::UnicodeWidthStr::width(out.as_str()) <= max,
                "max={max} width={}: [{out}]",
                unicode_width::UnicodeWidthStr::width(out.as_str())
            );
        }
    }

    #[test]
    fn test_truncate_zero_max_empty() {
        assert_eq!(truncate_width("anything", 0), "");
    }

    /// A query that is a command name + space resolves to that command's arg
    /// hint (the hint-after-space behavior) — e.g. "resume " → the file/sid hint.
    #[test]
    fn test_arg_hint_after_space() {
        assert!(
            arg_hint_for("resume ").is_some(),
            "resume + space should yield a hint"
        );
        assert!(
            arg_hint_for("export ").is_some(),
            "export + space should yield a hint"
        );
        assert_eq!(
            arg_hint_for("resume "),
            Some("<file.json | session name | sid>")
        );
    }

    /// A query that is not an arg-taking command yields no hint (the plain
    /// "no match" row shows instead).
    #[test]
    fn test_arg_hint_none() {
        assert_eq!(arg_hint_for("nope"), None);
        assert_eq!(arg_hint_for("status"), None);
    }
}
