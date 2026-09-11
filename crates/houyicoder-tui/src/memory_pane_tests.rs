//! Memory pane list, detail, filtering, controls, and command tests.

#![cfg(test)]

use houyicoder_protocol::frontend::SlashCommand;
use houyicoder_protocol::frontend::memory::ToggleState;

use crate::composition;
use crate::state::Pane;
use crate::test_support::render_text;

fn working() -> crate::state::App {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app
}

fn render(app: &crate::state::App) -> String {
    render_text(app, 100, 28)
}

/// List movement stays aligned with the filtered row selected on screen.
#[test]
fn test_memory_cursor_moves() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    // Cursor starts on row 0 (build-gate).
    let first = render(&app);
    assert!(first.contains("❯"), "cursor marker present");
    // Down to row 1 (comment-style).
    app.move_memory_cursor(1);
    let second = render(&app);
    assert!(
        second.contains("❯ [user] comment-style"),
        "cursor on comment-style:\n{second}"
    );
    // Up back to row 0.
    app.move_memory_cursor(-1);
    assert_eq!(app.memory.cursor(), 0, "cursor returns to 0");
    // Clamp: past the last row stays at the last.
    app.move_memory_cursor(100);
    assert_eq!(
        app.memory.cursor(),
        2,
        "cursor clamps to last row under All (3 entries)"
    );
    // Scope cycle resets the cursor to 0 (the filtered list changed).
    app.cycle_memory_scope();
    assert_eq!(app.memory.cursor(), 0, "scope cycle resets cursor");
}

/// Forget actions report the missing carrier in stub mode.
#[test]
fn test_memory_forget_no_carrier() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    app.move_memory_cursor(1);
    app.forget_memory_at_cursor();
    let d_out = render(&app);
    assert!(d_out.contains("no carrier"), "d action reports no carrier");
    // Command form: /memory forget <key>.
    app.run_tui_local_command("memory forget build-gate");
    let cmd_out = render(&app);
    assert!(cmd_out.contains("no carrier"), "command reports no carrier");
}

/// A refreshed MemoryList (the reply a forget / rescan sends) repopulates the
/// pane entries + resets the cursor so it never points past the new list.
#[test]
fn test_list_result_resets_cursor() {
    use crate::agent_message::AgentMessage;
    use houyicoder_protocol::frontend::memory::MemorySummaryEntry;
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    app.memory.set_cursor(5);
    let transcript_len = app.transcript.len();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    app.handle_agent_message(AgentMessage::MemoryListResult {
        entries: vec![MemorySummaryEntry {
            key: "fresh-gate".into(),
            description: "re-seeded".into(),
            source: "project".into(),
            scope: "project".into(),
            mtime_secs: now,
        }],
    });
    assert_eq!(app.memory.cursor(), 0, "cursor reset on refresh");
    assert!(app.memory.entries().iter().any(|m| m.topic == "fresh-gate"));
    assert!(render(&app).contains("project · now"));
    assert_eq!(app.transcript.len(), transcript_len);
    assert_eq!(app.pane, crate::state::Pane::Memory);
}

/// Pane navigation and actions route to the selected memory.
#[test]
fn test_memory_pane_keys_route() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    assert_eq!(app.memory.cursor(), 0);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.memory.cursor(), 1, "Down moves cursor");
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.memory.cursor(), 0, "Up moves cursor back");
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(
        app.memory.scope(),
        crate::state::enums::MemoryScopeTab::User
    );
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(app.memory.scope(), crate::state::enums::MemoryScopeTab::All);
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
    );
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
    );
    // d fires the forget action (stub mode reports no carrier).
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
    );
    let out = render(&app);
    assert!(out.contains("no carrier"), "d fires forget action");
}

#[test]
fn test_detail_soft_wrap_max() {
    let body = vec![ratatui::text::Line::from("x".repeat(25))];
    assert_eq!(crate::view::memory_pane::detail_max_offset(&body, 10, 2), 1);
}

#[test]
fn test_memory_detail_inline() {
    use crate::agent_message::AgentMessage;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::memory::MemoryDetail;
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    let transcript_len = app.transcript.len();
    let req_id = RequestId(7);
    app.memory.request_detail(req_id, "newest".into());
    app.handle_agent_message(AgentMessage::MemoryShowResult {
        req_id,
        entry: Some(MemoryDetail {
            key: "newest".into(),
            content: format!(
                "full memory body\n{}",
                (0..40)
                    .map(|index| format!("detail line {index}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            source: "feedback".into(),
            description: "full memory body".into(),
            mtime_secs: 1,
        }),
    });

    let detail = render(&app);
    assert!(detail.contains("newest"));
    assert_eq!(detail.matches("full memory body").count(), 1);
    assert!(detail.contains("detail line 0"));
    assert!(detail.contains("Esc to back"));
    assert_eq!(app.transcript.len(), transcript_len);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.memory.detail_offset(), Some(1));
    for _ in 0..100 {
        crate::keys::handle_working(
            &mut app,
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
        );
    }
    let bottom = app.memory.detail_offset().expect("detail open");
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.memory.detail_offset(), Some(bottom));

    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.memory.detail().is_none());
    assert_eq!(app.pane, crate::state::Pane::Memory);
}

#[test]
fn test_stale_detail_ignored() {
    use crate::agent_message::AgentMessage;
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::memory::MemoryDetail;
    let mut app = working();
    app.pane = Pane::Memory;
    app.memory.request_detail(RequestId(1), "old".into());
    app.memory.request_detail(RequestId(2), "new".into());
    app.handle_agent_message(AgentMessage::MemoryShowResult {
        req_id: RequestId(1),
        entry: Some(MemoryDetail {
            key: "old".into(),
            content: "stale".into(),
            source: "user".into(),
            description: String::new(),
            mtime_secs: 0,
        }),
    });
    assert_eq!(app.memory.pending_key(), Some("new"));
    assert!(!render(&app).contains("stale"));
}

/// /memory search <term> narrows the list to entries whose key or description
/// match (composed with the scope tab). Esc clears the filter. The stub seeds
/// build-gate / comment-style / spec-driven — "build" matches only build-gate.
#[test]
fn test_memory_search_narrows_clears() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = working();
    app.run_tui_local_command("memory search build");
    let narrowed = render(&app);
    assert!(
        narrowed.contains("1 stored"),
        "search narrows to one:\n{narrowed}"
    );
    assert!(narrowed.contains("search: [build]"), "search query shown");
    assert!(narrowed.contains("build-gate"), "matching entry shows");
    assert!(!narrowed.contains("comment-style"), "non-match hidden");
    // Esc clears the filter, back to the full set.
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let cleared = render(&app);
    assert!(cleared.contains("3 stored"), "Esc restores full list");
    assert!(!cleared.contains("search: ["), "search row gone after Esc");
}

/// Enter requests detail for the selected row.
#[test]
fn test_enter_shows_no_carrier() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let out = render(&app);
    assert!(out.contains("no carrier"), "enter fires show action");
}

#[test]
fn test_memory_pane_renders_rows() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    let on = render(&app);
    assert_eq!(app.pane, Pane::Memory);
    assert!(
        on.contains("a to disable auto-memory"),
        "auto-memory hint missing"
    );
    assert!(
        on.contains("c to disable auto-dream"),
        "auto-dream hint missing"
    );
    app.memory.set_toggles(ToggleState {
        auto_memory: false,
        auto_dream: false,
    });
    let off = render(&app);
    assert!(
        off.contains("a to enable auto-memory"),
        "auto-memory state missing"
    );
    assert!(
        off.contains("c to enable auto-dream"),
        "auto-dream state missing"
    );
}

#[test]
fn test_memory_toggle_no_carrier() {
    let mut app = working();
    let handled = app.run_tui_local_command("memory toggle auto");
    assert!(handled, "toggle subcommand handled");
    let out = render(&app);
    assert!(
        out.contains("no carrier"),
        "stub mode reports no carrier:\n{out}"
    );
}

#[test]
fn test_memory_toggle_bad_arg() {
    let mut app = working();
    let handled = app.run_tui_local_command("memory toggle bogus");
    assert!(handled, "bad toggle still handled");
    let out = render(&app);
    assert!(
        out.contains("/memory toggle auto|dream"),
        "usage names both switches:\n{out}"
    );
}

#[test]
fn test_memory_pane_empty_state() {
    let mut app = working();
    app.memory.clear_entries();
    app.run_command(SlashCommand::Memory);
    let out = render(&app);
    assert!(out.contains("0 stored"), "zero count missing:\n{out}");
    assert!(
        out.contains("no memories yet"),
        "empty hint missing:\n{out}"
    );
}

#[test]
fn test_memory_pane_renders_tag() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    let out = render_text(&app, 100, 28);
    assert!(
        out.contains("[project] build-gate"),
        "scope and key missing:\n{out}"
    );
    assert!(
        out.contains("project · unknown"),
        "source and age missing:\n{out}"
    );
}

#[test]
fn test_scope_tab_narrows_list() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    let all = render(&app);
    assert!(all.contains("3 stored"), "All shows all three:\n{all}");
    assert!(all.contains("[All]"), "All tab active");
    app.cycle_memory_scope();
    let user = render(&app);
    assert!(user.contains("1 stored"), "User narrows to one:\n{user}");
    assert!(user.contains("comment-style"), "user-scoped entry shows");
    assert!(
        !user.contains("build-gate"),
        "project-scoped hidden under User"
    );
    assert!(user.contains("[User]"), "User tab active");
    app.cycle_memory_scope();
    let project = render(&app);
    assert!(project.contains("build-gate"), "project-scoped shows");
    assert!(
        !project.contains("comment-style"),
        "user-scoped hidden under Project"
    );
    assert!(project.contains("[Project]"), "Project tab active");
    app.cycle_memory_scope();
    let auto = render(&app);
    assert!(auto.contains("spec-driven"), "auto-scoped shows");
    assert!(auto.contains("[Auto]"), "Auto tab active");
    app.cycle_memory_scope();
    let back = render(&app);
    assert!(back.contains("[All]"), "cycle wraps to All");
    assert!(back.contains("3 stored"), "All shows all three again");
}
