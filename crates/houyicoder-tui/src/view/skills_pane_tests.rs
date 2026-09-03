//! Skills pane rendering tests, extracted from agent_dispatch_tests on
//! size grounds.

use crate::composition;
use crate::state::{Pane, Screen};
use houyicoder_protocol::frontend::skills::{SkillEntry, SkillUsage};

fn render(app: &crate::state::App, w: u16, h: u16) -> String {
    crate::test_support::render_text(app, w, h)
}

#[test]
fn test_skills_pane_detail_renders() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.pane = Pane::Skills;
    app.skill_entries = vec![SkillEntry {
        name: "deep-review".into(),
        description: "the review standard".into(),
        origin: "project".into(),
        invocable: true,
        body_token_estimate: 2_100,
        usage: None,
    }];
    app.skill_level.set(1);
    let out = render(&app, 80, 24);
    assert!(out.contains("deep-review"), "name in detail: {out}");
    assert!(out.contains("the review standard"), "desc in detail: {out}");
    assert!(out.contains("origin: project"), "origin in detail: {out}");
    assert!(out.contains("Esc to back"), "back hint in detail: {out}");
}

#[test]
fn test_skills_pane_disabled_glyph() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.pane = Pane::Skills;
    app.skill_entries = vec![SkillEntry {
        name: "stale".into(),
        description: "a stale skill".into(),
        origin: "user".into(),
        invocable: true,
        body_token_estimate: 50,
        usage: None,
    }];
    app.skill_disabled.insert("stale".to_string());
    let out = render(&app, 80, 24);
    assert!(out.contains("○"), "disabled glyph renders: {out}");
    assert!(
        !out.contains("✓ stale"),
        "invocable glyph hidden when disabled: {out}"
    );
}

/// The detail view renders the usage line when usage data is present:
/// invocation count, refusals, last-used relative time, and token estimate.
#[test]
fn test_detail_usage_invoked() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.pane = Pane::Skills;
    app.skill_entries = vec![SkillEntry {
        name: "alpha".into(),
        description: "a skill".into(),
        origin: "user".into(),
        invocable: true,
        body_token_estimate: 100,
        usage: Some(SkillUsage {
            invocations: 3,
            refusals: 1,
            last_used_secs: 1,
        }),
    }];
    app.skill_level.set(1);
    let out = render(&app, 80, 24);
    assert!(out.contains("invoked 3"), "invocation count: {out}");
    assert!(out.contains("1 refused"), "refusal count: {out}");
    assert!(out.contains("this session"), "session label: {out}");
    assert!(out.contains("300 tok est."), "token estimate: {out}");
}

/// The detail view shows "never invoked this session" when usage is zero.
#[test]
fn test_detail_usage_never() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.pane = Pane::Skills;
    app.skill_entries = vec![SkillEntry {
        name: "beta".into(),
        description: "b skill".into(),
        origin: "user".into(),
        invocable: true,
        body_token_estimate: 50,
        usage: Some(SkillUsage::default()),
    }];
    app.skill_level.set(1);
    let out = render(&app, 80, 24);
    assert!(
        out.contains("never invoked this session"),
        "never-invoked label: {out}"
    );
}
