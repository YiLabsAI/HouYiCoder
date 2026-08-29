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
    app.fleet
        .entries
        .len()
        .min(MAX_VISIBLE)
        .min(u16::MAX as usize) as u16
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
    let len = app.fleet.entries.len();
    let start = app
        .fleet
        .selected
        .map(|s| s.min(len.saturating_sub(MAX_VISIBLE)))
        .unwrap_or(0);
    let end = (start + MAX_VISIBLE).min(len);
    app.fleet.entries[start..end]
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let abs_idx = start + i;
            build_row(entry, abs_idx == app.fleet.selected.unwrap_or(usize::MAX))
        })
        .collect()
}

/// One pill row. Completed children dim and go terse — the live verb drops,
/// leaving the type + a done marker + the token total — so a finished
/// delegation reads "explore · done · 1.2k tok" instead of echoing the stale
/// last verb. The running child keeps the verb and turn counter so the user
/// sees live progress at a glance.
fn build_row(entry: &FleetEntry, selected: bool) -> Line<'_> {
    let prefix = if selected { "> " } else { "  " };
    let (glyph, style) = if entry.completed.is_some() {
        ("✓ ", Style::default().fg(Color::DarkGray))
    } else {
        ("◯ ", Style::default().fg(Color::Cyan))
    };
    let tokens = format_tokens(entry.tokens);
    let body = if entry.completed.is_some() {
        format!("{} · done · {}", entry.subagent_type, tokens)
    } else {
        format!(
            "{}: {} · {} · turn {}",
            entry.subagent_type,
            verb_for(entry.last_activity.as_deref()),
            tokens,
            entry.turn
        )
    };
    Line::from(vec![
        Span::styled(prefix.to_string(), style),
        Span::styled(glyph.to_string(), style),
        Span::styled(body, style),
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
            completed_at: None,
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
        app.fleet.entries.push(entry("a", "explore", 1, 10, "grep"));
        assert_eq!(height(&app), 1);
        for i in 0..5 {
            app.fleet
                .entries
                .push(entry(&format!("b{i}"), "plan", 1, 10, "read"));
        }
        assert_eq!(height(&app), 3);
    }

    /// A completed row goes terse: it shows the type, a done marker, and the
    /// token total — never the stale live verb. Pins the auto-background
    /// render so a refactor that re-adds the verb to a finished row fails.
    #[test]
    fn test_completed_row_terse() {
        let e = FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "explore".into(),
            turn: 3,
            tokens: 1200,
            tool_uses: 2,
            last_activity: Some("grep".into()),
            completed: Some("completed".into()),
            completed_at: None,
        };
        let line = build_row(&e, false);
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("done"), "completed shows done: {text}");
        assert!(text.contains("1.2k tok"), "completed shows tokens: {text}");
        assert!(
            !text.contains("searching"),
            "completed drops the live verb: {text}"
        );
    }

    /// retire_completed drops a finished entry once its grace window elapsed.
    /// A completed_at six seconds ago is past the five-second grace, so the
    /// entry leaves the footer (the result stays in the transcript fold).
    #[test]
    fn test_retire_drops_expired() {
        use crate::agent_message::FleetState;
        use std::time::{Duration, Instant};
        let mut fleet = FleetState::default();
        fleet.entries.push(FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "explore".into(),
            turn: 3,
            tokens: 100,
            tool_uses: 1,
            last_activity: None,
            completed: Some("completed".into()),
            completed_at: Instant::now().checked_sub(Duration::from_secs(6)),
        });
        assert!(fleet.retire_completed(None), "expired entry retired");
        assert!(fleet.entries.is_empty(), "footer emptied after retire");
    }

    /// retire_completed keeps a running child (no completed_at) and a
    /// recently completed one (inside the grace window). The pill only
    /// leaves once the grace window elapses.
    #[test]
    fn test_retire_keeps_recent() {
        use crate::agent_message::FleetState;
        use std::time::Instant;
        let mut fleet = FleetState::default();
        fleet.entries.push(entry("a", "explore", 1, 10, "grep"));
        fleet.entries.push(FleetEntry {
            agent_id: "b".into(),
            subagent_type: "plan".into(),
            turn: 2,
            tokens: 50,
            tool_uses: 0,
            last_activity: None,
            completed: Some("completed".into()),
            completed_at: Some(Instant::now()),
        });
        assert!(
            !fleet.retire_completed(None),
            "nothing retired inside grace"
        );
        assert_eq!(fleet.entries.len(), 2, "running + recent both kept");
    }

    /// A selection pointing at a retired row clamps back into bounds rather
    /// than indexing past the end of the surviving entries.
    #[test]
    fn test_retire_clamps_selected() {
        use crate::agent_message::FleetState;
        use std::time::{Duration, Instant};
        let mut fleet = FleetState::default();
        fleet.entries.push(FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "explore".into(),
            turn: 1,
            tokens: 10,
            tool_uses: 0,
            last_activity: None,
            completed: Some("completed".into()),
            completed_at: Instant::now().checked_sub(Duration::from_secs(6)),
        });
        fleet.entries.push(entry("c2", "plan", 1, 10, "read"));
        fleet.selected = Some(0);
        assert!(fleet.retire_completed(None), "first entry retired");
        assert_eq!(fleet.selected, Some(0), "selection clamped to the survivor");
    }

    /// retain_viewed pins the child the user is drilled into: even past the
    /// grace window, the viewed child's row stays in the footer so it does
    /// not vanish while the user reads its transcript. A second completed
    /// child the user is not viewing still retires on schedule.
    #[test]
    fn test_retire_pins_viewed_child() {
        use crate::agent_message::FleetState;
        use std::time::{Duration, Instant};
        let mut fleet = FleetState::default();
        fleet.entries.push(FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "explore".into(),
            turn: 3,
            tokens: 100,
            tool_uses: 1,
            last_activity: None,
            completed: Some("completed".into()),
            completed_at: Instant::now().checked_sub(Duration::from_secs(6)),
        });
        fleet.entries.push(FleetEntry {
            agent_id: "c2".into(),
            subagent_type: "plan".into(),
            turn: 1,
            tokens: 10,
            tool_uses: 0,
            last_activity: None,
            completed: Some("completed".into()),
            completed_at: Instant::now().checked_sub(Duration::from_secs(6)),
        });
        assert!(
            fleet.retire_completed(Some("c1")),
            "non-viewed child retired, viewed kept"
        );
        assert_eq!(fleet.entries.len(), 1, "viewed child stays past grace");
        assert_eq!(fleet.entries[0].agent_id, "c1");
    }
}
