//! /memory pane cursor + forget interaction tests. Extracted from
//! interact_tests.rs so that file stays under the file-size gate. Each test
//! drives the App, renders to a TestBackend, and asserts on the real text.

#![cfg(test)]

use houyicoder_protocol::frontend::SlashCommand;

use crate::composition;
use crate::test_support::render_text;

fn working() -> crate::state::App {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app
}

fn render(app: &crate::state::App) -> String {
    render_text(app, 100, 28)
}

/// Up/Down move the cursor in the scope-filtered list; the cursor row shows
/// the marker so the user sees which row d/enter will hit. Clamps at the
/// ends. Pins the cursor + render + the shared filter (cursor index matches
/// the rendered row).
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
        second.contains("❯ [user·feedback] comment-style"),
        "cursor on comment-style:\n{second}"
    );
    // Up back to row 0.
    app.move_memory_cursor(-1);
    assert_eq!(app.memory_cursor, 0, "cursor returns to 0");
    // Clamp: past the last row stays at the last.
    app.move_memory_cursor(100);
    assert_eq!(
        app.memory_cursor, 2,
        "cursor clamps to last row under All (3 entries)"
    );
    // Scope cycle resets the cursor to 0 (the filtered list changed).
    app.cycle_memory_scope();
    assert_eq!(app.memory_cursor, 0, "scope cycle resets cursor");
}

/// The d action + the /memory forget command both report no carrier in stub
/// mode rather than crashing. Pins the forget wiring (the d key + the
/// command) + the None-carrier branch.
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
    app.memory_cursor = 5; // deliberately past the list end
    app.handle_agent_message(AgentMessage::MemoryListResult {
        entries: vec![MemorySummaryEntry {
            key: "fresh-gate".into(),
            description: "re-seeded".into(),
            source: "project".into(),
            scope: "project".into(),
            mtime_secs: 0,
        }],
    });
    assert_eq!(app.memory_cursor, 0, "cursor reset on refresh");
    assert!(app.memory_entries.iter().any(|m| m.topic == "fresh-gate"));
    assert_eq!(app.pane, crate::state::Pane::Memory);
}

/// Pressing Up/Down/d in the /memory pane moves the cursor + fires the d
/// action (no carrier in stub). Pins the key routing gated on the Memory pane.
#[test]
fn test_memory_pane_keys_route() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    assert_eq!(app.memory_cursor, 0);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.memory_cursor, 1, "Down moves cursor");
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.memory_cursor, 0, "Up moves cursor back");
    // d fires the forget action (stub mode reports no carrier).
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
    );
    let out = render(&app);
    assert!(out.contains("no carrier"), "d fires forget action");
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

/// enter on the cursor row fires the show action (no carrier in stub). Pins
/// the enter key routing for the inline body fetch.
#[test]
fn test_enter_shows_no_carrier() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let out = render(&app);
    assert!(out.contains("no carrier"), "enter fires show action");
}
