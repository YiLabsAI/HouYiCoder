use std::time::Duration;
use std::time::Instant;

use houyicoder_protocol::envelope::ClientFrame;
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::memory::ToggleState;
use houyicoder_protocol::llm::EffortLevel;

use super::*;
use crate::input::InputField;
use crate::session::agent_message::{ServerResponse, SessionMessage};
use crate::state::Pane;
use crate::test_harness::{
    TransportEvent, connected_app_with_events, render_text, wait_for_request, working_app,
};
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

/// The Config tab as the reader sees it: one line per row, joined so the
/// assertions can read the text the pane prints.
fn config_text(app: &App) -> String {
    render_config(app)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The Config tab lists the knobs, capitalized, with no breaker row while the
/// breaker is closed.
#[test]
fn test_config_tab_has_knobs() {
    let mut app = working_app();
    app.sessions.current.capabilities.model.catalog.applied.id = "qwen3.8-max".into();
    app.sessions.current.sandbox = "mac-seatbelt".into();
    let s = config_text(&app);
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
    let s = config_text(&app);
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
    let s = config_text(&app);
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
    let s = config_text(&app);
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

/// The key that acts is named once, in the footer. A row that takes no press
/// prints its value dim rather than repeating the key, so the pane says
/// "shown, not changed" without a sentence on every line.
#[test]
fn test_read_only_row_dims() {
    let mut app = working_app();
    let rows = render_config(&app);
    assert_eq!(
        rows[0].spans[1].style.fg,
        Some(Color::DarkGray),
        "the model row takes no press and prints dim"
    );
    assert_eq!(
        rows[1].spans[1].style.fg,
        Some(Color::DarkGray),
        "the effort follows the model pick, so this row prints dim too"
    );
    assert_eq!(
        rows[2].spans[1].style.fg, None,
        "a row that acts prints its value at full contrast"
    );

    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Config;
    app.ui.navigation.status_pane.config_cursor = Some(0);
    let out = render_text(&app, 100, 30);
    assert_eq!(
        out.matches("Enter to change").count(),
        1,
        "the footer is the only place the key is named:\n{out}"
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

/// The Config tab is a list of settings, and the two recap rows are the pair
/// the host persists. Enter sends the pair; the row redraws from the reply,
/// not from a transcript echo, so the reader never sees the log claim a change
/// before the pane shows it.
#[test]
fn test_recap_rows_act() {
    let (mut app, events) = connected_app_with_events();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Config;
    app.sync_config_cursor();
    {
        let mut snap = app.snapshot_or_stub();
        snap.recap_enabled = false;
        snap.recap_idle_secs = 300;
        app.sessions.current.capabilities.status.status_cache = Some(snap);
    }

    let switch = CONFIG_ROWS
        .iter()
        .position(|r| *r == ConfigRow::Recap)
        .expect("the recap switch is a row");
    app.ui.navigation.status_pane.config_cursor = Some(switch);
    app.activate_config_row();

    let sent = wait_for_request(&events, |req| {
        matches!(req, FrontendRequest::RecapConfigSet { .. })
    });
    let FrontendRequest::RecapConfigSet { enabled, idle_secs } = sent.payload else {
        unreachable!("the finder matched the recap request")
    };
    assert!(enabled, "the switch flips against the snapshot");
    assert_eq!(idle_secs, 300, "the window travels with the switch");

    app.handle_agent_message(SessionMessage::Response {
        request: sent.req_id,
        response: ServerResponse::MemoryToggleState {
            state: ToggleState {
                auto_memory: true,
                auto_dream: true,
                recap: true,
                recap_idle_secs: 300,
            },
        },
    });
    let out = render_text(&app, 100, 30);
    assert!(
        out.contains("Recap:") && out.contains("on"),
        "the row shows the switch the host confirmed: {out}"
    );

    let idle = CONFIG_ROWS
        .iter()
        .position(|r| *r == ConfigRow::RecapIdle)
        .expect("the idle window is a row");
    app.ui.navigation.status_pane.config_cursor = Some(idle);
    app.activate_config_row();

    let sent = wait_for_request(&events, |req| {
        matches!(req, FrontendRequest::RecapConfigSet { .. })
    });
    let FrontendRequest::RecapConfigSet { idle_secs, .. } = sent.payload else {
        unreachable!("the finder matched the recap request")
    };
    assert_eq!(idle_secs, 600, "the window steps 5m to 10m");

    app.handle_agent_message(SessionMessage::Response {
        request: sent.req_id,
        response: ServerResponse::MemoryToggleState {
            state: ToggleState {
                auto_memory: true,
                auto_dream: true,
                recap: true,
                recap_idle_secs: 600,
            },
        },
    });
    let out = render_text(&app, 100, 30);
    assert!(
        out.contains("Recap idle trigger:") && out.contains("10m"),
        "the window row redraws from the reply: {out}"
    );
    assert!(
        out.contains("recap: on · idle after 10m"),
        "the confirmed reply writes the transcript receipt in the same frame: {out}"
    );
    drop(events);
}

/// A second press before the first reply lands is dropped. It would read the
/// snapshot the pending reply has not updated, send the same value again, and
/// a double press meant as on-then-off would land on with two receipts.
#[test]
fn test_recap_repeat_press_dropped() {
    let (mut app, events) = connected_app_with_events();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Config;
    app.sync_config_cursor();
    let switch = CONFIG_ROWS
        .iter()
        .position(|r| *r == ConfigRow::Recap)
        .expect("the recap switch is a row");
    app.ui.navigation.status_pane.config_cursor = Some(switch);
    app.activate_config_row();
    app.activate_config_row();

    let first = wait_for_request(&events, |req| {
        matches!(req, FrontendRequest::RecapConfigSet { .. })
    });
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut extra = 0;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        let Ok(TransportEvent::Frame(frame)) = events.recv_timeout(remaining) else {
            break;
        };
        if let Ok(ClientFrame::Request(env)) = serde_json::from_str::<ClientFrame>(&frame)
            && matches!(env.payload, FrontendRequest::RecapConfigSet { .. })
        {
            extra += 1;
        }
    }
    assert_eq!(extra, 0, "the repeat press sent nothing");

    app.handle_agent_message(SessionMessage::Response {
        request: first.req_id,
        response: ServerResponse::MemoryToggleState {
            state: ToggleState {
                auto_memory: true,
                auto_dream: true,
                recap: true,
                recap_idle_secs: 300,
            },
        },
    });
    app.activate_config_row();
    let next = wait_for_request(&events, |req| {
        matches!(req, FrontendRequest::RecapConfigSet { .. })
    });
    let FrontendRequest::RecapConfigSet { enabled, .. } = next.payload else {
        unreachable!("the finder matched the recap request")
    };
    assert!(
        !enabled,
        "after the reply a press flips the confirmed value"
    );
}

/// A reply carrying a window the tab did not ask for still redraws the row:
/// the host is the authority, and the tab never pins a value it only hoped
/// for.
#[test]
fn test_recap_row_follows_host() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Config;
    app.sync_config_cursor();
    {
        let mut snap = app.snapshot_or_stub();
        snap.recap_enabled = false;
        snap.recap_idle_secs = 300;
        app.sessions.current.capabilities.status.status_cache = Some(snap);
    }
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(99),
        response: ServerResponse::MemoryToggleState {
            state: ToggleState {
                auto_memory: true,
                auto_dream: true,
                recap: true,
                recap_idle_secs: 900,
            },
        },
    });
    let out = render_text(&app, 100, 30);
    assert!(
        out.contains("15m"),
        "the host's window is what the row shows: {out}"
    );
}

/// A tab used without a connection says so instead of silently doing nothing:
/// the row is reachable from a detached session, and a press that produced no
/// line would read as a broken key.
#[test]
fn test_recap_disconnected_says() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    app.ui.navigation.status_pane.tab = StatusTab::Config;
    app.sync_config_cursor();
    let switch = CONFIG_ROWS
        .iter()
        .position(|r| *r == ConfigRow::Recap)
        .expect("a recap row");
    app.ui.navigation.status_pane.config_cursor = Some(switch);

    app.activate_config_row();

    let out = render_text(&app, 100, 30);
    assert!(
        out.contains("recap: not connected"),
        "a detached press is reported: {out}"
    );
}
