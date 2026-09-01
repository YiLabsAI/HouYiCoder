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

use crate::agent_message::{FleetEntry, FleetState};
use crate::state::App;

/// Max child rows the pill ever shows. A longer fleet scrolls within the
/// pill so the transcript is never pushed off-screen.
const MAX_VISIBLE: usize = 3;

/// Rows the pill would like: one per child, capped so a large fleet scrolls
/// within the pill instead of pushing the transcript off-screen. Zero when
/// the fleet is empty. The shared footer budget decides what it gets.
pub fn want(app: &App) -> u16 {
    app.fleet
        .entries
        .len()
        .min(MAX_VISIBLE)
        .min(u16::MAX as usize) as u16
}

/// Draw the pill into the granted area. One row for a multi-child fleet is
/// the collapsed form: a single row of per-child detail would name one child
/// and silently omit the rest, so it becomes a count instead.
pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let lines = if area.height == 1 && app.fleet.entries.len() > 1 {
        vec![summary_line(app)]
    } else {
        build_lines(app)
    };
    f.render_widget(Paragraph::new(lines), area);
}

/// The collapsed form: how many children are live and how many just landed.
fn summary_line(app: &App) -> Line<'static> {
    let done = app
        .fleet
        .entries
        .iter()
        .filter(|e| e.completed.is_some())
        .count();
    let running = app.fleet.entries.len() - done;
    let mut text = format!("  {running} running");
    if done > 0 {
        text.push_str(&format!(" \u{00b7} {done} done"));
    }
    Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))
}

/// First entry index the pill shows. When the fleet is longer than
/// MAX_VISIBLE the window slides so the selected row stays on screen. The
/// click router maps a row back through the same start, so the row clicked
/// is the entry acted on.
pub fn window_start(app: &App) -> usize {
    window_start_idx(app.fleet.selected, app.fleet.entries.len())
}

/// What a click in the strip does. The summary row of a multi-child fleet
/// names no child, so it opens the pane — the surface its own hint names.
/// A child row selects; the already-selected row drills in.
#[derive(Debug)]
pub enum FleetClick {
    OpenAgentsPane,
    Select(usize),
    Drill(String),
}

/// Pure routing of a click at the strip's given row. Kept free of App so
/// the decision is testable without one; the mouse handler applies it.
pub fn click_route(fleet: &FleetState, granted: u16, row: usize) -> FleetClick {
    let n = fleet.entries.len();
    if granted == 1 && n > 1 {
        return FleetClick::OpenAgentsPane;
    }
    let idx = window_start_idx(fleet.selected, n) + row;
    let Some(entry) = fleet.entries.get(idx) else {
        return FleetClick::OpenAgentsPane;
    };
    if fleet.selected == Some(idx) {
        FleetClick::Drill(entry.agent_id.clone())
    } else {
        FleetClick::Select(idx)
    }
}

/// The sliding window's start as a free function, so click_route and the
/// draw share one definition without either needing an App.
fn window_start_idx(selected: Option<usize>, len: usize) -> usize {
    selected
        .map(|s| s.min(len.saturating_sub(MAX_VISIBLE)))
        .unwrap_or(0)
}

/// Build the visible window of pill rows.
fn build_lines(app: &App) -> Vec<Line<'_>> {
    let len = app.fleet.entries.len();
    let start = window_start(app);
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

    /// The wanted height tracks the fleet up to the cap; a one-entry fleet
    /// asks for one row, a five-entry fleet still asks for three.
    #[test]
    fn test_want_caps_at_three() {
        let mut app = composition::app();
        assert_eq!(want(&app), 0);
        app.fleet.entries.push(entry("a", "explore", 1, 10, "grep"));
        assert_eq!(want(&app), 1);
        for i in 0..5 {
            app.fleet
                .entries
                .push(entry(&format!("b{i}"), "plan", 1, 10, "read"));
        }
        assert_eq!(want(&app), 3);
    }

    /// One row for a multi-child fleet is the collapsed form: a count, not
    /// the first child's detail. Naming one child and omitting the rest is
    /// worse than not naming any, since nothing tells the reader the others
    /// exist.
    #[test]
    fn test_one_row_counts() {
        let mut app = composition::app();
        app.fleet.entries.push(entry("a", "explore", 1, 10, "grep"));
        app.fleet.entries.push(entry("b", "plan", 1, 10, "read"));
        let mut done = entry("c", "verify", 1, 10, "test");
        done.completed = Some("completed".into());
        done.completed_at = Some(std::time::Instant::now());
        app.fleet.entries.push(done);
        let line = summary_line(&app);
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(
            text.contains("2 running") && text.contains("1 done"),
            "the collapsed row counts instead of naming: {text}"
        );
        assert!(
            !text.contains("explore"),
            "no single child is named in the collapsed row: {text}"
        );
    }

    /// Click routing: a row selects, the selected row drills in, and the
    /// multi-child summary opens the pane (its click target names no child,
    /// so it must not act on one). An out-of-range row falls back to the pane
    /// rather than ignoring the click outright.
    #[test]
    fn test_click_route() {
        let fleet = |selected: Option<usize>| FleetState {
            entries: vec![
                entry("a", "explore", 1, 10, "grep"),
                entry("b", "plan", 1, 10, "read"),
            ],
            selected,
            ..Default::default()
        };
        match click_route(&fleet(None), 2, 1) {
            FleetClick::Select(1) => {}
            other => panic!("unselected row selects: {other:?}"),
        }
        match click_route(&fleet(Some(1)), 2, 1) {
            FleetClick::Drill(sid) => assert_eq!(sid, "b"),
            other => panic!("the selected row drills in: {other:?}"),
        }
        match click_route(&fleet(Some(0)), 1, 0) {
            FleetClick::OpenAgentsPane => {}
            other => panic!("summary row opens the pane: {other:?}"),
        }
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
