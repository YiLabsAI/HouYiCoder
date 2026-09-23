//! Skills pane rendering tests.

use crossterm::event::KeyCode;
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::skills::{SkillEntry, SkillUsage};

use crate::composition;
use crate::state::{Pane, Screen};

fn render(app: &crate::state::App, w: u16, h: u16) -> String {
    crate::test_harness::render_text(app, w, h)
}

fn key(code: KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
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
    app.skills_pane.open_unavailable("deep-review".into());
    let out = render(&app, 80, 24);
    assert!(out.contains("deep-review"), "name in detail: {out}");
    assert!(out.contains("the review standard"), "desc in detail: {out}");
    assert!(
        out.contains("Native — .houyicoder/skills/"),
        "origin label and path in detail: {out}"
    );
    assert!(out.contains("Esc to back"), "back hint in detail: {out}");
}

/// A description wider than the pane wraps onto the following rows. A row
/// rendered as one logical line clips everything past the right edge, so the
/// tail never becomes reachable by scrolling.
#[test]
fn test_detail_wraps_long_description() {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.pane = Pane::Skills;
    app.skill_entries = vec![SkillEntry {
        name: "find-skills".into(),
        description: "Helps users discover and install agent skills when they ask questions like \
                      how do I do X, find a skill for X, or express interest in"
            .into(),
        origin: "agents".into(),
        invocable: true,
        body_token_estimate: 1_368,
        user_invocable: true,
        usage: None,
    }];
    app.skills_pane.open_unavailable("find-skills".into());
    let out = render(&app, 60, 24);
    assert!(
        out.contains("express interest in"),
        "the description tail wraps into view instead of clipping: {out}"
    );
}

/// The detail renders the skill body, and a body taller than its region
/// scrolls with the arrow keys while the header and usage stay pinned.
#[test]
fn test_detail_body_scrolls() {
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
            invocations: 2,
            refusals: 0,
            last_used_secs: 1,
        }),
    }];
    let body = (0..40)
        .map(|index| format!("body line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.skills_pane.request_detail(RequestId(4), "alpha".into());
    assert!(app.skills_pane.apply_detail(RequestId(4), Some(body)));
    let top = render(&app, 80, 24);
    assert!(top.contains("body line 0"), "body renders: {top}");
    assert!(top.contains("invoked 2"), "usage pinned: {top}");
    crate::keys::handle_working(&mut app, key(KeyCode::PageDown));
    let scrolled = render(&app, 80, 24);
    assert!(
        !scrolled.contains("body line 0") && scrolled.contains("body line 10"),
        "the body advances while the header stays: {scrolled}"
    );
    assert!(
        scrolled.contains("invoked 2"),
        "the usage line stays pinned while the body scrolls: {scrolled}"
    );
}

/// A skill whose body cannot be resolved opens the detail with an
/// unavailable note rather than dropping the view.
#[test]
fn test_detail_body_unavailable() {
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
        usage: None,
    }];
    app.skills_pane.request_detail(RequestId(1), "alpha".into());
    assert!(app.skills_pane.apply_detail(RequestId(1), None));
    let out = render(&app, 80, 24);
    assert!(out.contains("body unavailable"), "unavailable note: {out}");
    assert!(out.contains("alpha"), "the header still renders: {out}");
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
    // The detail opened from that row shows the same disabled glyph, so the
    // header never claims a skill is usable while the toggle says otherwise.
    app.skills_pane.open_unavailable("stale".into());
    let detail = render(&app, 80, 24);
    assert!(detail.contains("○"), "disabled glyph in detail: {detail}");
    assert!(
        !detail.contains("✓"),
        "detail must not show the usable glyph: {detail}"
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
    app.skills_pane.open_unavailable("alpha".into());
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
    app.skills_pane.open_unavailable("beta".into());
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
    app.skills_pane.open_unavailable("managed".into());
    let out = render(&app, 80, 24);
    assert!(out.contains("✓"), "usable glyph in detail: {out}");
    assert!(
        !out.contains("✗"),
        "detail must not show disabled glyph: {out}"
    );
}
