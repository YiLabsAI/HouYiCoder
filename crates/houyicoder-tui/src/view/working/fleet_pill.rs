//! Footer fleet pill: one row per spawned child, shown only while the
//! fleet is non-empty. Each row carries the child's hollow-circle glyph,
//! its type, a verb inferred from the last tool, and cumulative tokens.
//! Shift-arrow moves App.fleet_selected; the selected row gets a prefix
//! and Enter drills into its teammate view. The pill caps at three rows
//! and scrolls toward the selection when the fleet is longer.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::agent_message::FleetEntry;
use crate::state::App;

/// Max child rows the pill ever shows. A longer fleet scrolls within the
/// pill so the transcript is never pushed off-screen.
const MAX_VISIBLE: usize = 3;

/// Height the working layout should reserve for the pill. Zero when the
/// fleet is empty so the pill vanishes and the input box sits right under
/// the transcript.
pub fn height(app: &App) -> u16 {
    app.fleet.len().min(MAX_VISIBLE).min(u16::MAX as usize) as u16
}

/// Draw the pill into the reserved area. The caller reserves height only
/// while the fleet is non-empty, so no empty/guard branch is needed here.
pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let lines = build_lines(app);
    f.render_widget(Paragraph::new(lines), area);
}

/// Build the visible window of pill rows. When the fleet is longer than
/// MAX_VISIBLE the window slides so the selected row stays on screen.
fn build_lines(app: &App) -> Vec<Line<'_>> {
    let len = app.fleet.len();
    let start = app
        .fleet_selected
        .map(|s| s.min(len.saturating_sub(MAX_VISIBLE)))
        .unwrap_or(0);
    let end = (start + MAX_VISIBLE).min(len);
    app.fleet[start..end]
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let abs_idx = start + i;
            build_row(entry, abs_idx == app.fleet_selected.unwrap_or(usize::MAX))
        })
        .collect()
}

/// One pill row. Completed children dim; the running child carries the
/// verb and token count so the user sees live progress at a glance.
fn build_row(entry: &FleetEntry, selected: bool) -> Line<'_> {
    let prefix = if selected { "> " } else { "  " };
    let (glyph, style) = if entry.completed.is_some() {
        ("✓ ", Style::default().fg(Color::DarkGray))
    } else {
        ("◯ ", Style::default().fg(Color::Cyan))
    };
    let verb = verb_for(entry.last_activity.as_deref());
    let tokens = format_tokens(entry.tokens);
    let tail = if entry.completed.is_some() {
        format!(" · {} · done", tokens)
    } else {
        format!(" · {} · turn {}", tokens, entry.turn)
    };
    Line::from(vec![
        Span::styled(prefix.to_string(), style),
        Span::styled(glyph.to_string(), style),
        Span::styled(format!("{}: ", entry.subagent_type), style),
        Span::styled(verb.to_string(), style),
        Span::styled(tail, style),
    ])
}

/// Infer a one-word status verb from the child's last tool name. The bus
/// carries the tool name verbatim; the pill humanizes it so the row reads
/// "explore: searching" rather than "explore: grep".
fn verb_for(tool: Option<&str>) -> &'static str {
    match tool {
        Some("read") | Some("glob") => "reading",
        Some("grep") | Some("search") => "searching",
        Some("edit") | Some("write") => "writing",
        Some("bash") => "building",
        Some("test") => "verifying",
        Some(_) => "working",
        None => "thinking",
    }
}

/// Compact token count: under 1k as-is, otherwise kilo with one decimal so
/// a row stays narrow. Matches the observability token-unit discipline.
fn format_tokens(tokens: u64) -> String {
    if tokens < 1000 {
        format!("{} tok", tokens)
    } else if tokens < 1_000_000 {
        format!("{:.1}k tok", tokens as f64 / 1000.0)
    } else {
        format!("{:.1}m tok", tokens as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composition;

    fn entry(id: &str, kind: &str, turn: u32, tokens: u64, tool: &str) -> FleetEntry {
        FleetEntry {
            agent_id: id.into(),
            subagent_type: kind.into(),
            turn,
            tokens,
            tool_uses: 0,
            last_activity: Some(tool.into()),
            completed: None,
        }
    }

    /// A grep tool reads as "searching", not the raw tool name.
    #[test]
    fn test_verb_from_tool() {
        assert_eq!(verb_for(Some("grep")), "searching");
        assert_eq!(verb_for(Some("edit")), "writing");
        assert_eq!(verb_for(Some("bash")), "building");
        assert_eq!(verb_for(None), "thinking");
    }

    /// Sub-1k tokens render plain; 1k+ renders kilo with one decimal so the
    /// row width stays bounded.
    #[test]
    fn test_token_format_compact() {
        assert_eq!(format_tokens(50), "50 tok");
        assert_eq!(format_tokens(1200), "1.2k tok");
    }

    /// Height tracks the fleet up to the cap; a one-entry fleet reserves
    /// one row, a five-entry fleet still reserves three.
    #[test]
    fn test_height_caps_at_three() {
        let mut app = composition::app();
        assert_eq!(height(&app), 0);
        app.fleet.push(entry("a", "explore", 1, 10, "grep"));
        assert_eq!(height(&app), 1);
        for i in 0..5 {
            app.fleet
                .push(entry(&format!("b{i}"), "plan", 1, 10, "read"));
        }
        assert_eq!(height(&app), 3);
    }
}
