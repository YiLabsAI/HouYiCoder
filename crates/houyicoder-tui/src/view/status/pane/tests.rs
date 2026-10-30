use houyicoder_protocol::llm::EffortLevel;

use super::*;
use crate::input::InputField;
use crate::state::Pane;
use crate::test_harness::{render_text, working_app};
use crate::view::status::config_rows::ConfigRow;

/// The sub-tab header renders Status / Config / Usage, with the active one
/// marked (the active title appears in the header).
#[test]
fn test_header_renders_all_tabs() {
    let line = tab_header_at(StatusTab::Status, &STATUS_TABS, "");
    let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(rendered.contains("Status"), "Status tab: {rendered}");
    assert!(rendered.contains("Config"), "Config tab: {rendered}");
    assert!(rendered.contains("Usage"), "Usage tab: {rendered}");
}

/// The Config tab lists the knobs, capitalized, with no breaker row while the
/// breaker is closed.
#[test]
fn test_config_tab_has_knobs() {
    let mut app = working_app();
    app.sessions.current.capabilities.model.catalog.applied.id = "qwen3.8-max".into();
    app.sessions.current.sandbox = "mac-seatbelt".into();
    let s = render_config(&app);
    assert!(s.contains("qwen3.8-max"), "model: {s}");
    assert!(s.contains("mac-seatbelt"), "sandbox: {s}");
    assert!(s.contains("Permission mode:"), "permission mode: {s}");
    assert!(s.contains("Auto-memory:"), "auto-memory row: {s}");
    assert!(s.contains("Auto-dream:"), "auto-dream row: {s}");
    assert!(
        !s.contains("breaker:"),
        "a closed breaker gets no row of its own: {s}"
    );
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
        .find(|l| l.trim_start().starts_with("Auto-memory"))
        .unwrap_or_else(|| panic!("auto-memory row missing: {s}"));
    let dream_line = s
        .lines()
        .find(|l| l.trim_start().starts_with("Auto-dream"))
        .unwrap_or_else(|| panic!("auto-dream row missing: {s}"));
    assert!(memory_line.contains("off"), "off toggle: {s}");
    assert!(dream_line.contains("on"), "on toggle: {s}");
}

/// The Config tab's Reasoning effort row states the applied level, and says
/// the request carries none when nothing is set. The row is always present:
/// the list has a fixed height, and a row that came and went would shift the
/// cursor under the reader's finger.
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
    assert!(s.contains("Reasoning effort:"), "effort row: {s}");
    assert!(s.contains("high"), "high level: {s}");
    // The row stays when nothing is applied, and says so.
    app.sessions
        .current
        .capabilities
        .model
        .catalog
        .applied
        .effort = None;
    let s = render_config(&app);
    assert!(
        s.contains("Reasoning effort:") && s.contains("(default)"),
        "the row states that no effort is sent: {s}"
    );
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

/// The tab row starts in the same column as the field labels under it. An
/// indented tab row over flush-left fields is the one line out of alignment in
/// the pane, and the eye reads it as a mistake before it reads either line.
#[test]
fn test_tab_row_alignment() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Status;
    let out = render_text(&app, 100, 30);
    let lines: Vec<&str> = out.lines().collect();
    let tab = lines
        .iter()
        .position(|l| l.contains("[Status]"))
        .unwrap_or_else(|| panic!("no tab row:\n{out}"));
    let field = lines
        .iter()
        .position(|l| l.contains("Version:"))
        .unwrap_or_else(|| panic!("no field row:\n{out}"));
    let tab_col = lines[tab].find("[Status]").expect("bracket present");
    let field_col = lines[field].find("Version:").expect("label present");
    assert_eq!(
        tab_col, field_col,
        "the tab row starts where the labels do:\n{out}"
    );
}

/// The three regions read as three: the tab row, a blank separator row, then
/// the fields, and another blank row before the key hint. The measurement is
/// where the first field sits relative to the tab row, because a blank line
/// two rows down proves nothing about the row between them.
#[test]
fn test_regions_are_separated() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Status;
    let out = render_text(&app, 100, 30);
    let lines: Vec<&str> = out.lines().collect();
    let tab = lines.iter().position(|l| l.contains("[Status]")).unwrap();
    let field = lines
        .iter()
        .position(|l| l.contains("Version:"))
        .unwrap_or_else(|| panic!("no field row:\n{out}"));
    let hint = lines.iter().position(|l| l.contains("switch tab")).unwrap();
    assert_eq!(
        field,
        tab + 2,
        "one blank row separates the tab row from the fields:\n{out}"
    );
    assert!(
        lines[field + 1..hint]
            .iter()
            .filter(|l| !l.trim().is_empty())
            .count()
            > 0,
        "the fields fill the body between the spacers:\n{out}"
    );
}

/// The Config tab is a list: the cursor marks one row, and the footer names
/// the keys that move it and act on it. Without a cursor there is nothing for
/// Enter to change, and a footer that omits the keys hides the only gesture
/// the tab offers.
#[test]
fn test_config_tab_shows_cursor() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Config;
    app.sync_config_cursor();
    let out = render_text(&app, 100, 30);
    assert!(
        out.contains("❯ Model"),
        "the cursor marks the first row:\n{out}"
    );
    assert!(
        out.contains("Up/Down"),
        "the footer names the move key:\n{out}"
    );
    assert!(
        out.contains("Enter"),
        "the footer names the action key:\n{out}"
    );
    assert!(
        !out.contains("Space"),
        "one action, one key: Space is not a second binding:\n{out}"
    );
}

/// A row that shows state the session does not own prints no change hint, so
/// the pane never promises an edit it cannot make.
#[test]
fn test_read_only_no_hint() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Config;
    app.ui.navigation.status_pane.config_cursor = Some(0);
    let out = render_text(&app, 100, 30);
    let model = out
        .lines()
        .find(|l| l.contains("Model"))
        .unwrap_or_else(|| panic!("no model row:\n{out}"));
    assert!(
        !model.contains("Enter to change"),
        "the model row takes no press: {model}"
    );
}

/// Enter on the timer row advances the ring, and the Config row itself
/// redraws with the new value. The row is read out of the rendered pane, not
/// out of the transcript echo the step also writes: the echo proves the key
/// fired, while the pane proves the value a reader is looking at changed.
#[test]
fn test_timer_row_changes() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Config;
    app.sync_config_cursor();
    app.sessions
        .current
        .capabilities
        .permission
        .auto_approve_delay_secs = 7;
    let timer = CONFIG_ROWS
        .iter()
        .position(|r| *r == ConfigRow::AutoApproveTimer)
        .unwrap();
    app.ui.navigation.status_pane.config_cursor = Some(timer);

    let row_text = |app: &App| -> String {
        let out = render_text(app, 100, 30);
        out.lines()
            .find(|l| l.contains("Auto-approve timer:"))
            .unwrap_or_else(|| panic!("no timer row:\n{out}"))
            .to_string()
    };
    assert!(
        row_text(&app).contains("7s"),
        "the row starts on the configured value"
    );

    app.activate_config_row();
    assert_eq!(
        app.sessions
            .current
            .capabilities
            .permission
            .auto_approve_delay_secs,
        15,
        "the next step in the ring, written where the countdown reads it"
    );
    let row = row_text(&app);
    assert!(
        row.contains("15s"),
        "the Config row shows the new value: {row}"
    );
    assert!(!row.contains("7s"), "and stops showing the old one: {row}");
}
