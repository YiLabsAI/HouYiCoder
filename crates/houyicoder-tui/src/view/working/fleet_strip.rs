//! Footer fleet strip: one row per spawned child, shown only while the
//! fleet is non-empty. Each row carries the child's hollow-circle glyph,
//! its type, a verb inferred from the last tool, and cumulative tokens.
//! Shift-arrow moves the strip's selection; the selected row gets a prefix
//! and Enter drills into its teammate view. The strip caps at three rows
//! and scrolls toward the selection when the fleet is longer.

use std::time::Duration;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::agent_message::FleetEntry;
use crate::state::App;

/// Max child rows the strip ever shows. A longer fleet scrolls within the
/// strip so the transcript is never pushed off-screen.
const MAX_VISIBLE: usize = 3;

/// Rows the strip would like: one per child, capped so a large fleet scrolls
/// within the strip instead of pushing the transcript off-screen. Zero when
/// the fleet is empty. The shared footer budget decides what it gets.
pub fn want(app: &App) -> u16 {
    let entries = &app.sessions.current.capabilities.fleet.entries;
    entries.len().min(MAX_VISIBLE).min(u16::MAX as usize) as u16
}

/// Draw the strip into the granted area. One row for a multi-child fleet is
/// the collapsed form: a single row of per-child detail would name one child
/// and silently omit the rest, so it becomes a count instead.
pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let entries = &app.sessions.current.capabilities.fleet.entries;
    let lines = if area.height == 1 && entries.len() > 1 {
        vec![summary_line(app)]
    } else {
        build_lines(app, area.height as usize)
    };
    f.render_widget(Paragraph::new(lines), area);
}

/// The collapsed form: how many children are live and how many just landed.
fn summary_line(app: &App) -> Line<'static> {
    let entries = &app.sessions.current.capabilities.fleet.entries;
    let done = entries.iter().filter(|e| e.completed.is_some()).count();
    let running = entries.len() - done;
    let mut text = format!("  {running} running");
    if done > 0 {
        text.push_str(&format!(" \u{00b7} {done} done"));
    }
    Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))
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
/// the decision is testable without one; the mouse handler applies it. The
/// entries are the session's fleet facts and the selection is the strip's
/// interaction state, so the caller passes both.
pub fn click_route(
    entries: &[FleetEntry],
    selected: Option<usize>,
    granted: u16,
    row: usize,
) -> FleetClick {
    let n = entries.len();
    let g = granted as usize;
    if g == 1 && n > 1 {
        return FleetClick::OpenAgentsPane;
    }
    let visible = visible_rows(n, g);
    let idx = window_start_idx(selected, n, visible) + row;
    let Some(entry) = entries.get(idx) else {
        return FleetClick::OpenAgentsPane;
    };
    if selected == Some(idx) {
        FleetClick::Drill(entry.agent_id.clone())
    } else {
        FleetClick::Select(idx)
    }
}

/// The sliding window's start as a free function, so click_route and the
/// draw share one definition without either needing an App.
fn window_start_idx(selected: Option<usize>, len: usize, visible: usize) -> usize {
    selected
        .map(|s| s.min(len.saturating_sub(visible)))
        .unwrap_or(0)
}

/// Rows of the sliding window: the granted height capped at the fleet size.
/// The strip spends every row it gets on a child rather than on a "+N more"
/// line. Such a line is misleading in a sliding window — the hidden count is
/// constant, but once the user scrolls the hidden rows sit above the window,
/// so "+N more" reads as "below" when the tail is already in view. The user
/// discovers more rows by scrolling; completed entries drop in the grace
/// window, so a hidden completed row is ephemeral.
fn visible_rows(len: usize, granted: usize) -> usize {
    granted.min(len)
}

/// Build the sliding window of strip rows, capped by the granted height.
/// The window follows the selection so the highlighted row stays on screen;
/// the strip spends every row it gets on a child rather than on an
/// overflow indicator (see visible_rows for why that line is dropped).
fn build_lines(app: &App, granted: usize) -> Vec<Line<'_>> {
    let entries = &app.sessions.current.capabilities.fleet.entries;
    let selected = app.ui.fleet.selected;
    let len = entries.len();
    if len == 0 || granted == 0 {
        return Vec::new();
    }
    let visible = visible_rows(len, granted);
    let start = window_start_idx(selected, len, visible);
    let end = (start + visible).min(len);
    entries[start..end]
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let abs_idx = start + i;
            build_row(entry, abs_idx == selected.unwrap_or(usize::MAX))
        })
        .collect()
}

/// One strip row. Completed children dim and go terse — the live verb drops,
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
        let elapsed = entry
            .started_at
            .map(|t| format!(" · {}", format_elapsed(t.elapsed())))
            .unwrap_or_default();
        format!(
            "{}: {} · {} · turn {}{}",
            entry.subagent_type,
            verb_for(entry.last_activity.as_deref()),
            tokens,
            entry.turn,
            elapsed
        )
    };
    Line::from(vec![
        Span::styled(prefix.to_string(), style),
        Span::styled(glyph.to_string(), style),
        Span::styled(body, style),
    ])
}

/// Infer a one-word status verb from the child's last tool name. The bus
/// carries the tool name verbatim; the strip humanizes it so the row reads
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

/// Compact elapsed: under 60s as "Ns", under 1h as "Nm Ms", otherwise
/// "Nh Nm Ns" so a long-running child does not show "312s" and retains
/// the finer unit at each scale.
fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        format!("{}h {}m {}s", h, m, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composition;
    use crate::session::Session;

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
            started_at: None,
        }
    }

    /// The session's fleet entries, for test setup.
    fn entries_mut(session: &mut Session) -> &mut Vec<FleetEntry> {
        &mut session.capabilities.fleet.entries
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
        entries_mut(&mut app.sessions.current).push(entry("a", "explore", 1, 10, "grep"));
        assert_eq!(want(&app), 1);
        for i in 0..5 {
            entries_mut(&mut app.sessions.current).push(entry(
                &format!("b{i}"),
                "plan",
                1,
                10,
                "read",
            ));
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
        entries_mut(&mut app.sessions.current).push(entry("a", "explore", 1, 10, "grep"));
        entries_mut(&mut app.sessions.current).push(entry("b", "plan", 1, 10, "read"));
        let mut done = entry("c", "verify", 1, 10, "test");
        done.completed = Some("completed".into());
        done.completed_at = Some(std::time::Instant::now());
        entries_mut(&mut app.sessions.current).push(done);
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

    /// The sliding window follows the selection so the highlighted row
    /// stays on screen, and never spends a row on a "+N more" line — the
    /// user discovers hidden rows by scrolling, and the strip sets no
    /// overflow indicator. With five agents and three granted rows, an
    /// unset selection shows the head (t0,t1,t2); a tail selection slides
    /// the window (t2,t3,t4); a head selection shows the head again. No
    /// row is ever a "+N" indicator.
    #[test]
    fn test_window_follows_selection() {
        let mut app = composition::app();
        for (i, kind) in ["t0", "t1", "t2", "t3", "t4"].iter().enumerate() {
            entries_mut(&mut app.sessions.current).push(entry(
                &format!("a{i}"),
                kind,
                1,
                10,
                "grep",
            ));
        }
        let text = |app: &App| -> Vec<String> {
            build_lines(app, 3)
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        let head = text(&app);
        assert_eq!(head.len(), 3, "three rows, no +N row");
        assert!(
            head[0].contains("t0"),
            "head window starts at t0: {}",
            head[0]
        );
        assert!(
            !head.iter().any(|s| s.contains("more")),
            "no +N row: {head:?}"
        );

        app.ui.fleet.selected = Some(4);
        let tail = text(&app);
        assert_eq!(tail.len(), 3);
        assert!(
            tail[0].contains("t2"),
            "tail window slid to t2: {}",
            tail[0]
        );
        assert!(tail[2].contains("t4"), "selection t4 in view: {}", tail[2]);
        assert!(
            !tail.iter().any(|s| s.contains("more")),
            "no +N at tail: {tail:?}"
        );

        app.ui.fleet.selected = Some(0);
        let back = text(&app);
        assert!(back[0].contains("t0"), "head window restored: {}", back[0]);
    }

    /// When the fleet fits the granted height, every row is a child — the
    /// window covers the whole fleet and there is nothing to scroll past.
    #[test]
    fn test_window_fits_granted() {
        let mut app = composition::app();
        entries_mut(&mut app.sessions.current).push(entry("a", "explore", 1, 10, "grep"));
        entries_mut(&mut app.sessions.current).push(entry("b", "plan", 1, 10, "read"));
        let lines = build_lines(&app, 3);
        assert_eq!(lines.len(), 2, "two children fill two of three rows");
    }

    /// Click routing: a row selects, the selected row drills in, and the
    /// multi-child summary opens the pane (its click target names no child,
    /// so it must not act on one). An out-of-range row falls back to the pane
    /// rather than ignoring the click outright.
    #[test]
    fn test_click_route() {
        let entries = vec![
            entry("a", "explore", 1, 10, "grep"),
            entry("b", "plan", 1, 10, "read"),
        ];
        match click_route(&entries, None, 2, 1) {
            FleetClick::Select(1) => {}
            other => panic!("unselected row selects: {other:?}"),
        }
        match click_route(&entries, Some(1), 2, 1) {
            FleetClick::Drill(sid) => assert_eq!(sid, "b"),
            other => panic!("the selected row drills in: {other:?}"),
        }
        match click_route(&entries, Some(0), 1, 0) {
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
            started_at: None,
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

    /// drop_completed drops a finished entry once its grace window elapsed.
    /// A completed_at six seconds ago is past the five-second grace, so the
    /// entry leaves the footer (the result stays in the transcript fold).
    #[test]
    fn test_grace_drops_expired() {
        use crate::session::capabilities::FleetCapability;
        use std::time::{Duration, Instant};
        let mut fleet = FleetCapability::default();
        fleet.entries.push(FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "explore".into(),
            turn: 3,
            tokens: 100,
            tool_uses: 1,
            last_activity: None,
            completed: Some("completed".into()),
            completed_at: Instant::now().checked_sub(Duration::from_secs(6)),
            started_at: None,
        });
        assert!(fleet.drop_completed(None), "expired entry dropped");
        assert!(fleet.entries.is_empty(), "footer emptied after drop");
    }

    /// drop_completed keeps a running child (no completed_at) and a
    /// recently completed one (inside the grace window). The strip only
    /// leaves once the grace window elapses.
    #[test]
    fn test_grace_keeps_recent() {
        use crate::session::capabilities::FleetCapability;
        use std::time::Instant;
        let mut fleet = FleetCapability::default();
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
            started_at: None,
        });
        assert!(!fleet.drop_completed(None), "nothing dropped inside grace");
        assert_eq!(fleet.entries.len(), 2, "running + recent both kept");
    }

    /// A selection pointing at a dropped row clamps back into bounds rather
    /// than indexing past the end of the surviving entries. Exercised through
    /// the App composition so the capability's retain and the strip's clamp
    /// cannot drift at separate call sites: the second phase selects the tail
    /// row and drops it, which only lands on the survivor when the clamp runs
    /// inside the seam.
    #[test]
    fn test_drop_clamps_selected() {
        use std::time::{Duration, Instant};
        let mut app = composition::app();
        entries_mut(&mut app.sessions.current).push(FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "explore".into(),
            turn: 1,
            tokens: 10,
            tool_uses: 0,
            last_activity: None,
            completed: Some("completed".into()),
            completed_at: Instant::now().checked_sub(Duration::from_secs(6)),
            started_at: None,
        });
        entries_mut(&mut app.sessions.current).push(entry("c2", "plan", 1, 10, "read"));
        app.ui.fleet.selected = Some(0);
        assert!(app.drop_completed_fleet(), "first entry dropped");
        assert_eq!(
            app.ui.fleet.selected,
            Some(0),
            "selection clamped to the survivor"
        );

        entries_mut(&mut app.sessions.current).push(FleetEntry {
            agent_id: "c3".into(),
            subagent_type: "verify".into(),
            turn: 1,
            tokens: 10,
            tool_uses: 0,
            last_activity: None,
            completed: Some("completed".into()),
            completed_at: Instant::now().checked_sub(Duration::from_secs(6)),
            started_at: None,
        });
        app.ui.fleet.selected = Some(1);
        assert!(app.drop_completed_fleet(), "expired tail dropped");
        assert_eq!(
            app.ui.fleet.selected,
            Some(0),
            "a selection past the survivor clamps back"
        );
    }

    /// retain_viewed pins the child the user is drilled into: even past the
    /// grace window, the viewed child's row stays in the footer so it does
    /// not vanish while the user reads its transcript. A second completed
    /// child the user is not viewing still drops on schedule.
    #[test]
    fn test_grace_pins_viewed_child() {
        use crate::session::capabilities::FleetCapability;
        use std::time::{Duration, Instant};
        let mut fleet = FleetCapability::default();
        fleet.entries.push(FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "explore".into(),
            turn: 3,
            tokens: 100,
            tool_uses: 1,
            last_activity: None,
            completed: Some("completed".into()),
            completed_at: Instant::now().checked_sub(Duration::from_secs(6)),
            started_at: None,
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
            started_at: None,
        });
        assert!(
            fleet.drop_completed(Some("c1")),
            "non-viewed child dropped, viewed kept"
        );
        assert_eq!(fleet.entries.len(), 1, "viewed child stays past grace");
        assert_eq!(fleet.entries[0].agent_id, "c1");
    }

    /// tick_fleet_elapsed dirties at most once per second while running
    /// children exist; returns false when idle and resets so the next spawn
    /// ticks immediately. Exercised through the App composition so the
    /// capability's running check and the strip's pacing cannot drift at
    /// separate call sites.
    #[test]
    fn test_tick_elapsed() {
        use std::time::{Duration, Instant};
        let mut app = composition::app();
        let now = Instant::now();
        assert!(!app.tick_fleet_elapsed(now), "no running children");
        entries_mut(&mut app.sessions.current).push(entry("a", "explore", 1, 10, "grep"));
        assert!(app.tick_fleet_elapsed(now), "first tick after spawn");
        assert!(
            !app.tick_fleet_elapsed(now),
            "same instant does not re-tick"
        );
        assert!(
            app.tick_fleet_elapsed(now + Duration::from_secs(1)),
            "1s later re-ticks"
        );
        entries_mut(&mut app.sessions.current)[0].completed = Some("done".into());
        assert!(
            !app.tick_fleet_elapsed(now + Duration::from_secs(2)),
            "no running children after completion"
        );
        entries_mut(&mut app.sessions.current)[0].completed = None;
        assert!(
            app.tick_fleet_elapsed(now + Duration::from_millis(100)),
            "reset ticks immediately on next spawn"
        );
    }

    /// A running child with started_at renders the elapsed seconds so the
    /// user sees how long it has been running.
    #[test]
    fn test_running_row_shows_elapsed() {
        use std::time::{Duration, Instant};
        let e = FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "explore".into(),
            turn: 2,
            tokens: 100,
            tool_uses: 0,
            last_activity: Some("grep".into()),
            completed: None,
            completed_at: None,
            started_at: Instant::now().checked_sub(Duration::from_secs(7)),
        };
        let line = build_row(&e, false);
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("7s"), "running row shows elapsed: {text}");
        assert!(!text.contains("done"), "running row is not terse: {text}");
    }

    #[test]
    fn test_format_elapsed_compact() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_elapsed(Duration::from_secs(312)), "5m 12s");
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1h 0m 0s");
        assert_eq!(format_elapsed(Duration::from_secs(3725)), "1h 2m 5s");
    }
}
