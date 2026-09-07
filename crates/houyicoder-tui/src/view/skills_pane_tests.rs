//! Skills pane rendering tests.

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
        user_invocable: true,
        usage: None,
    }];
    app.skill_level.set(1);
    let out = render(&app, 80, 24);
    assert!(out.contains("deep-review"), "name in detail: {out}");
    assert!(out.contains("the review standard"), "desc in detail: {out}");
    assert!(out.contains("origin: Native"), "origin in detail: {out}");
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
        user_invocable: true,
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
        user_invocable: true,
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
        user_invocable: true,
        usage: Some(SkillUsage::default()),
    }];
    app.skill_level.set(1);
    let out = render(&app, 80, 24);
    assert!(
        out.contains("never invoked this session"),
        "never-invoked label: {out}"
    );
}

/// A skill with model-invocation disabled but user-invocation enabled
/// (invocable=false, user_invocable=true) is still usable — the glyph is
/// ✓ in both the listing and the detail view. This is the mixed case the
/// user_invocable field exists to handle; without it the old invocable-only
/// gate showed ✗ and the toggle refused to act.
#[test]
fn test_user_only_skill_glyph() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.pane = Pane::Skills;
    app.skill_entries = vec![SkillEntry {
        name: "managed".into(),
        description: "user-only skill".into(),
        origin: "managed".into(),
        invocable: false,
        body_token_estimate: 50,
        user_invocable: true,
        usage: None,
    }];
    // Listing view: checkmark, not cross.
    let out = render(&app, 80, 24);
    assert!(out.contains("✓"), "usable glyph in listing: {out}");
    assert!(!out.contains("✗"), "must not show disabled glyph: {out}");
    // Detail view: also checkmark.
    app.skill_level.set(1);
    let out = render(&app, 80, 24);
    assert!(out.contains("✓"), "usable glyph in detail: {out}");
    assert!(
        !out.contains("✗"),
        "detail must not show disabled glyph: {out}"
    );
}
