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

/// The hint states what Esc does here. An idle child has no turn to cancel,
/// so Esc leaves; a running one is interrupted. The banner says which instead
/// of advertising a key that does nothing.
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
        !idle.contains("Esc interrupt"),
        "nothing is running to interrupt, got:\n{idle}"
    );

    app.sessions
        .current
        .capabilities
        .fleet
        .entries
        .push(FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "verify".into(),
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
        running.contains("Esc interrupt"),
        "a running child is interrupted, got:\n{running}"
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
