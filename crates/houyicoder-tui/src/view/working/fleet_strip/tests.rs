use super::*;
use crate::composition;
use crate::session::Session;
use crate::session::capabilities::FleetCapability;
use std::time::{Duration, Instant};

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

/// The wanted height tracks the fleet up to the cap; a one-entry fleet
/// asks for one row, a five-entry fleet still asks for three.
#[test]
fn test_want_caps_at_three() {
    let mut app = composition::app();
    assert_eq!(want(&app), 0);
    entries_mut(&mut app.sessions.current).push(entry("a", "explore", 1, 10, "grep"));
    assert_eq!(want(&app), 1);
    for i in 0..5 {
        entries_mut(&mut app.sessions.current).push(entry(&format!("b{i}"), "plan", 1, 10, "read"));
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
    done.completed_at = Some(Instant::now());
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
        entries_mut(&mut app.sessions.current).push(entry(&format!("a{i}"), kind, 1, 10, "grep"));
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
/// token total — never the stale live verb. Guards the auto-background
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
