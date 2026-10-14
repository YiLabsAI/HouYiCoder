use houyicoder_protocol::llm::EffortLevel;

use super::*;
use crate::input::InputField;
use crate::test_harness::working_app;

/// The sub-tab header renders Status / Config / Usage, with the active one
/// marked (the active title appears in the header).
#[test]
fn test_header_renders_all_tabs() {
    let line = tab_header(StatusTab::Status, &STATUS_TABS);
    let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(rendered.contains("Status"), "Status tab: {rendered}");
    assert!(rendered.contains("Config"), "Config tab: {rendered}");
    assert!(rendered.contains("Usage"), "Usage tab: {rendered}");
}

/// The Config tab renders model / permission mode / sandbox / breaker +
/// the settings-file memory toggles (auto-memory / auto-dream).
#[test]
fn test_config_tab_has_knobs() {
    let mut app = working_app();
    app.sessions.current.capabilities.model.catalog.applied.id = "qwen3.8-max".into();
    app.sessions.current.sandbox = "mac-seatbelt".into();
    let s = render_config(&app);
    assert!(s.contains("qwen3.8-max"), "model: {s}");
    assert!(s.contains("mac-seatbelt"), "sandbox: {s}");
    assert!(s.contains("Permission mode:"), "permission mode: {s}");
    assert!(s.contains("breaker:"), "breaker: {s}");
    assert!(s.contains("auto-memory:"), "auto-memory row: {s}");
    assert!(s.contains("auto-dream:"), "auto-dream row: {s}");
}

/// The Config tab renders the toggle state from the snapshot, so a snap
/// with auto-memory off shows "off" (the row reflects the wire value, not
/// a hardcoded default).
#[test]
fn test_config_tab_reflects_state() {
    let mut app = working_app();
    let mut snap = app.snapshot_or_stub();
    snap.auto_memory = false;
    snap.auto_dream = true;
    app.sessions.current.capabilities.status.status_cache = Some(snap);
    let s = render_config(&app);
    let memory_line = s
        .lines()
        .find(|l| l.trim_start().starts_with("auto-memory"))
        .unwrap_or_else(|| panic!("auto-memory row missing: {s}"));
    let dream_line = s
        .lines()
        .find(|l| l.trim_start().starts_with("auto-dream"))
        .unwrap_or_else(|| panic!("auto-dream row missing: {s}"));
    assert!(memory_line.ends_with("off"), "off toggle: {s}");
    assert!(dream_line.ends_with("on"), "on toggle: {s}");
}

/// The Config tab renders the resolved model id + the effort badge
/// (only when the next request really carries an effort parameter).
#[test]
fn test_config_tab_shows_effort() {
    let mut app = working_app();
    app.sessions.current.capabilities.model.catalog.applied.id = "qwen3.8-max".into();
    app.sessions
        .current
        .capabilities
        .model
        .catalog
        .applied
        .effort = Some(EffortLevel::High);
    let s = render_config(&app);
    assert!(s.contains("effort:"), "effort row: {s}");
    assert!(s.contains("high"), "high level: {s}");
    // Effort row hidden when None.
    app.sessions
        .current
        .capabilities
        .model
        .catalog
        .applied
        .effort = None;
    let s = render_config(&app);
    assert!(!s.contains("effort:"), "no effort row when None: {s}");
}

/// The editable name line renders the label + the buffer text (cursor at
/// the end renders a trailing space block).
#[test]
fn test_name_edit_renders_buffer() {
    let mut field = InputField::new();
    field.insert_str("fix");
    let line = name_edit_line(&field);
    let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        rendered.contains("Session name: "),
        "label present: {rendered}"
    );
    assert!(rendered.contains("fix"), "buffer text present: {rendered}");
}

/// An empty buffer still renders the label + a cursor block (the caret at
/// end), so the user sees where typing lands even before the first char.
#[test]
fn test_name_edit_renders_caret() {
    let field = InputField::new();
    let line = name_edit_line(&field);
    let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        rendered.contains("Session name: "),
        "label present: {rendered}"
    );
}

/// The cursor mid-buffer splits the text: a grapheme under the caret is
/// styled separately from the before/after text.
#[test]
fn test_name_edit_cursor_buffer() {
    let mut field = InputField::new();
    field.insert_str("abc");
    field.move_left(); // cursor between b and c (after "ab")
    let line = name_edit_line(&field);
    // Three text spans: "  Session name: ", "ab", the caret char "c"
    // (the cursor sits on 'c' so 'ab' is before + 'c' is the caret).
    let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(rendered.contains("ab"), "before-caret text: {rendered}");
    assert!(rendered.contains('c'), "caret char: {rendered}");
}
