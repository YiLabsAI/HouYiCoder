use std::time::Duration;
use std::time::Instant;

use crate::agent_message::FLEET_GRACE;
use crate::agent_message::FleetEntry;
use crate::composition::app;
use crate::records::TeammateView;
use crate::records::TranscriptLine;
use crate::state::Screen;
use crate::test_harness::render_text;
use crate::test_harness::{render_buffer, working_app};
use ratatui::style::Color;

#[test]
fn test_banner_shows_type() {
    let mut app = app();
    app.ui.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        prompt: "find auth".into(),
        color: None,
        transcript: vec![],
        ..Default::default()
    });
    let view = app.ui.teammate_view.as_ref().unwrap();
    let name = if view.subagent_type.is_empty() {
        "agent"
    } else {
        view.subagent_type.as_str()
    };
    assert_eq!(name, "explore");
}

#[test]
fn test_banner_type_fallback() {
    let mut app = app();
    app.ui.teammate_view = Some(TeammateView {
        child_sid: "c2".into(),
        subagent_type: String::new(),
        prompt: String::new(),
        color: None,
        transcript: vec![],
        ..Default::default()
    });
    let view = app.ui.teammate_view.as_ref().unwrap();
    let name = if view.subagent_type.is_empty() {
        "agent"
    } else {
        view.subagent_type.as_str()
    };
    assert_eq!(name, "agent");
}

/// The banner renders above the transcript when a teammate is in view,
/// naming the agent plus the gesture that leaves. Pins draw_banner against a
/// refactor that drops the line or the hint.
#[test]
fn test_banner_renders_name_hint() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        prompt: "find auth".into(),
        color: None,
        transcript: vec![TranscriptLine::Agent("child reply".into())],
        ..Default::default()
    });
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Viewing @explore"),
        "banner names the agent, got:\n{out}"
    );
    assert!(
        out.contains("Esc return"),
        "banner states how to leave, got:\n{out}"
    );
    assert!(
        out.contains("find auth"),
        "banner carries the dim summary line, got:\n{out}"
    );
    assert!(
        out.contains("child reply"),
        "swapped child transcript renders, got:\n{out}"
    );
}

/// No banner renders when no teammate is in view, so the transcript takes
/// the whole top. Pins the no-banner path against a regression that
/// reserves a blank line.
#[test]
fn test_no_banner_without_teammate() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    let out = render_text(&app, 80, 24);
    assert!(
        !out.contains("Viewing @"),
        "no banner when teammate_view is None, got:\n{out}"
    );
}

/// A badge color on the viewed teammate is carried into the banner so the
/// name is distinguishable from siblings. Pins the color wiring against a
/// regression that drops it.
#[test]
fn test_banner_carries_badge_color() {
    let mut app = working_app();
    app.ui.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        subagent_type: "verify".into(),
        prompt: "ran deep review".into(),
        color: Some("red".into()),
        transcript: vec![],
        ..Default::default()
    });
    let buf = render_buffer(&app, 80, 24);
    // The banner is the only row carrying an "@"; assert the @ cell is
    // red so the badge color carried through to the banner name.
    let hit = buf
        .content()
        .iter()
        .any(|c| c.fg == Color::Red && c.symbol() == "@");
    assert!(
        hit,
        "the @verify name carries the red badge color, got buffer with no red @ cell"
    );
}

/// The hint names the two keys that work in the view and what each does. Esc
/// leaves the view in both states, so it reads the same; the stop key appears
/// only while a child is running, because only then is there a turn to stop.
/// A banner that offered the stop key on a finished child would advertise an
/// action with no target.
#[test]
fn test_banner_hint_follows_state() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        subagent_type: "verify".into(),
        prompt: "run the review".into(),
        ..Default::default()
    });
    let idle = render_text(&app, 80, 24);
    assert!(
        idle.contains("Esc return"),
        "an idle child is left with Esc, got:\n{idle}"
    );
    assert!(
        !idle.contains("ctrl+x"),
        "nothing is running to stop, got:\n{idle}"
    );

    app.sessions
        .current
        .capabilities
        .fleet
        .entries
        .push(FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "verify".into(),
            description: String::new(),
            turn: 1,
            tokens: 0,
            tool_uses: 0,
            last_activity: None,
            completed: None,
            completed_at: None,
            started_at: None,
        });
    let running = render_text(&app, 80, 24);
    assert!(
        running.contains("ctrl+x stop"),
        "a running child names the stop key, got:\n{running}"
    );
    assert!(
        running.contains("Esc return"),
        "and still names the way out, got:\n{running}"
    );
}

/// A multi-line prompt with markdown headings stays one row: a heading drawn
/// where the banner expects prose pushes the hint off the line.
#[test]
fn test_banner_prompt_single_line() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        subagent_type: "verify".into(),
        prompt: "## First: look\nThen report".into(),
        ..Default::default()
    });
    let out = render_text(&app, 90, 24);
    assert!(
        !out.contains("## First"),
        "the heading marks are dropped, got:\n{out}"
    );
    assert!(
        out.contains("First: look Then report"),
        "the prompt reads as one line, got:\n{out}"
    );
}

/// A finished child's footer row is pinned only while the reader is inside its
/// view. Without restarting the goodbye window on exit, the row drops on the
/// very next frame, because its original completion time is long past the
/// window. Stepping back would erase the row the reader just left.
#[test]
fn test_exit_restarts_finished_grace() {
    let mut app = working_app();
    let long_ago = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .expect("a minute ago");
    app.sessions
        .current
        .capabilities
        .fleet
        .entries
        .push(FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "explore".into(),
            description: String::new(),
            turn: 3,
            tokens: 100,
            tool_uses: 0,
            last_activity: None,
            completed: Some("completed".into()),
            completed_at: Some(long_ago),
            started_at: None,
        });
    app.ui.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        ..Default::default()
    });

    app.exit_teammate_view();

    let entry = app.fleet_entry("c1").expect("the row survives the exit");
    let at = entry.completed_at.expect("a finished child has a stamp");
    assert!(
        at.elapsed() < FLEET_GRACE,
        "the goodbye window starts when the reader leaves, not when the child finished"
    );
    assert!(
        !app.drop_completed_fleet(),
        "so the row stays on the next frame"
    );
}

/// The window is restarted only for the child just viewed. Another finished
/// child keeps its own clock, so stepping out of one view cannot resurrect a
/// row that expired while the reader was elsewhere.
#[test]
fn test_exit_restarts_only_viewed() {
    let mut app = working_app();
    let long_ago = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .expect("a minute ago");
    for id in ["c1", "c2"] {
        app.sessions
            .current
            .capabilities
            .fleet
            .entries
            .push(FleetEntry {
                agent_id: id.into(),
                subagent_type: "explore".into(),
                description: String::new(),
                turn: 3,
                tokens: 100,
                tool_uses: 0,
                last_activity: None,
                completed: Some("completed".into()),
                completed_at: Some(long_ago),
                started_at: None,
            });
    }
    app.ui.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        ..Default::default()
    });

    app.exit_teammate_view();

    let other = app.fleet_entry("c2").expect("the other row is still there");
    assert_eq!(
        other.completed_at,
        Some(long_ago),
        "an unviewed child keeps the clock it had"
    );
}

/// A child still running has no clock to restart, so leaving its view must not
/// invent one: a completion stamp on a running child would make it look
/// finished and drop the row after the window.
#[test]
fn test_exit_keeps_running_clock() {
    let mut app = working_app();
    app.sessions
        .current
        .capabilities
        .fleet
        .entries
        .push(FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "explore".into(),
            description: String::new(),
            turn: 3,
            tokens: 100,
            tool_uses: 0,
            last_activity: Some("grep".into()),
            completed: None,
            completed_at: None,
            started_at: None,
        });
    app.ui.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        ..Default::default()
    });

    app.exit_teammate_view();

    let entry = app.fleet_entry("c1").expect("a running child stays");
    assert!(
        entry.completed_at.is_none(),
        "a running child gains no completion stamp"
    );
}

/// The banner names the key that leaves whatever the child is doing, and names
/// the stop key only while there is a turn to stop. A finished view has no
/// turn left, so offering the stop key would promise an action with no target.
#[test]
fn test_banner_hint_matches_state() {
    let mut app = working_app();
    app.sessions
        .current
        .capabilities
        .fleet
        .entries
        .push(FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "verify".into(),
            description: String::new(),
            turn: 2,
            tokens: 100,
            tool_uses: 1,
            last_activity: Some("grep".into()),
            completed: None,
            completed_at: None,
            started_at: None,
        });
    app.ui.teammate_view = Some(TeammateView {
        child_sid: "c1".into(),
        subagent_type: "verify".into(),
        ..Default::default()
    });
    let running = render_text(&app, 100, 24);
    let title = running
        .lines()
        .find(|l| l.contains("Viewing @verify"))
        .unwrap_or_else(|| panic!("no banner:\n{running}"));
    assert!(title.contains("running"), "the state is named: {title}");
    assert!(
        title.contains("Esc return"),
        "the way out is named: {title}"
    );
    assert!(
        title.contains("ctrl+x stop"),
        "and so is the way to stop it: {title}"
    );

    app.sessions.current.capabilities.fleet.entries[0].completed = Some("completed".into());
    let done = render_text(&app, 100, 24);
    let title = done
        .lines()
        .find(|l| l.contains("Viewing @verify"))
        .unwrap_or_else(|| panic!("no banner:\n{done}"));
    assert!(
        title.contains("completed"),
        "the finished state is named: {title}"
    );
    assert!(
        title.contains("Esc return"),
        "the way out is still named: {title}"
    );
    assert!(
        !title.contains("ctrl+x"),
        "a finished view offers no stop key: {title}"
    );
}
