use super::*;
use crate::composition::app;
use crate::state::Screen;
use crate::test_harness::render_text;
use houyicoder_protocol::frontend::skills::SkillEntry;
use unicode_width::UnicodeWidthStr;

#[test]
fn test_row_spans() {
    let entry = SkillEntry {
        name: "ego-browser".to_string(),
        description: "browse the web".to_string(),
        origin: "project".to_string(),
        invocable: true,
        body_token_estimate: 120,
        user_invocable: true,
        usage: None,
    };
    let line = format_one(&entry, 40);
    assert!(line.spans.len() >= 4, "glyph + name + sep + desc");
}

/// A skill with model-invocation disabled but user-invocation enabled
/// shows the usable glyph (checkmark), not the disabled glyph (cross).
/// This is the mixed case the user_invocable field exists to handle.
#[test]
fn test_user_only_glyph() {
    let entry = SkillEntry {
        name: "managed".to_string(),
        description: "user-only".to_string(),
        origin: "managed".to_string(),
        invocable: false,
        body_token_estimate: 50,
        user_invocable: true,
        usage: None,
    };
    let line = format_one(&entry, 40);
    let glyph = &line.spans[0].content;
    assert!(
        glyph.contains("✓"),
        "usable glyph for user-only skill: {glyph}"
    );
}

/// A long name + long description must not overflow a narrow inner width:
/// the description clips to 0 (or near-0), not the border.
#[test]
fn test_long_name_clips_desc() {
    let entry = SkillEntry {
        name: "a-very-long-skill-name-that-eats-width".to_string(),
        description: "a description that would overflow".to_string(),
        origin: "user".to_string(),
        invocable: true,
        body_token_estimate: 100,
        user_invocable: true,
        usage: None,
    };
    let inner_w = 30u16;
    let line = format_one(&entry, inner_w);
    // The description span text width must fit within inner_w minus the
    // reserved prefix (glyph + space + name + separator + highlight).
    let name_w = UnicodeWidthStr::width(entry.name.as_str());
    let reserved = 1 + 1 + name_w + 2 + 2;
    let max_desc = (inner_w as usize).saturating_sub(reserved);
    let desc_span = &line.spans[4];
    let desc_w = UnicodeWidthStr::width(&*desc_span.content);
    assert!(
        desc_w <= max_desc,
        "desc width {desc_w} must fit budget {max_desc}"
    );
}

/// The picker renders the skill name, the gate glyph, and the nav hint
/// when open on the working screen.
#[test]
fn test_picker_renders_open() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.capabilities.skills.entries = vec![SkillEntry {
        name: "alpha".to_string(),
        description: "does alpha".to_string(),
        origin: "user".to_string(),
        invocable: true,
        body_token_estimate: 100,
        user_invocable: true,
        usage: None,
    }];
    app.ui.navigation.skill_picker.open = true;
    let out = render_text(&app, 80, 24);
    assert!(out.contains("alpha"), "skill name in picker: {out}");
    assert!(out.contains("Enter to insert"), "nav hint in picker: {out}");
}

/// The picker shows the empty-state line when no skills are discovered.
#[test]
fn test_picker_empty_state() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.skill_picker.open = true;
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("no skills match"),
        "empty state in picker: {out}"
    );
}

/// The picker filters skills by the text typed after @.
#[test]
fn test_picker_filter() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.capabilities.skills.entries = vec![
        SkillEntry {
            name: "alpha".to_string(),
            description: "a".to_string(),
            origin: "user".to_string(),
            invocable: true,
            body_token_estimate: 100,
            user_invocable: true,
            usage: None,
        },
        SkillEntry {
            name: "beta".to_string(),
            description: "b".to_string(),
            origin: "user".to_string(),
            invocable: true,
            body_token_estimate: 200,
            user_invocable: true,
            usage: None,
        },
    ];
    app.ui.navigation.skill_picker.open = true;
    app.ui.input.editor.push('@');
    app.ui.input.editor.push('b');
    let out = render_text(&app, 80, 24);
    assert!(out.contains("beta"), "filter matches beta: {out}");
    assert!(!out.contains("alpha"), "alpha filtered out: {out}");
}

/// The picker is not visible when closed (no title leaks onto the screen).
#[test]
fn test_picker_hidden_closed() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.capabilities.skills.entries = vec![SkillEntry {
        name: "alpha".to_string(),
        description: "does alpha".to_string(),
        origin: "user".to_string(),
        invocable: true,
        body_token_estimate: 100,
        user_invocable: true,
        usage: None,
    }];
    let out = render_text(&app, 80, 24);
    assert!(
        !out.contains("Enter to insert"),
        "picker title must not render when closed: {out}"
    );
}
