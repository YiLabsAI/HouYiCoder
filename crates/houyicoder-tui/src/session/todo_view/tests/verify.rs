//! Flow-level verification for the checklist render and the /status command.
//! These drive the real view::draw -> working::draw_transcript pipeline (the
//! checklist renders as selectable rows at the transcript tail) and the real
//! command dispatch (not pure-fn helpers), with a checklist view model
//! populated the way the wire accumulator would leave it.

use houyicoder_protocol::frontend::SlashCommand;

use crate::composition;
use crate::state::App;
use crate::state::Screen;
use crate::test_harness::render_text;
use crate::todo_view::{TodoStatus, TodoView};
use std::time::Duration;
use std::time::Instant;

/// Build the view list the wire accumulator would leave behind: the render
/// reads this exact cache, so direct construction drives the same view::draw
/// path a parsed todo-write frame would.
fn seeded_todos(items: &[(&str, TodoStatus)]) -> Vec<TodoView> {
    items
        .iter()
        .map(|(c, s)| TodoView {
            content: c.to_string(),
            status: *s,
            active_form: (*s == TodoStatus::InProgress).then(|| format!("doing {c}")),
        })
        .collect()
}

fn working_app() -> App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app
}

#[test]
fn test_collapsed_renders_active_footer() {
    let todos = seeded_todos(&[
        ("setup", TodoStatus::Completed),
        ("run tests", TodoStatus::InProgress),
        ("docs a", TodoStatus::Pending),
        ("docs b", TodoStatus::Pending),
        ("docs c", TodoStatus::Pending),
    ]);
    let mut app = working_app();
    app.sessions.current.todos.items = todos;
    // A fresh completion timestamp keeps the done item visible (30s TTL).
    app.sessions
        .current
        .todos
        .completion_at
        .insert("setup".into(), Instant::now());
    let out = render_text(&app, 80, 24);
    // The active item uses the cyan glyph and the active-form label.
    assert!(out.contains("◼"), "active glyph missing:\n{out}");
    assert!(
        out.contains("doing run tests"),
        "active-form missing:\n{out}"
    );
    // The recently completed item shows the done glyph.
    assert!(out.contains("✔"), "done glyph missing:\n{out}");
    // Two pending items are hidden beyond the three visible slots. The footer
    // names them with the same word the header above uses, and names the chord
    // that shows them, since it is the only thing saying more rows exist.
    assert!(out.contains("… hidden:"), "hidden footer missing:\n{out}");
    assert!(
        out.contains("+2 open"),
        "the hidden count uses the header's word:\n{out}"
    );
    assert!(
        out.contains("ctrl+o to expand"),
        "the footer names the expand chord:\n{out}"
    );
}

#[test]
fn test_collapsed_no_footer_visible() {
    let todos = seeded_todos(&[
        ("done", TodoStatus::Completed),
        ("next", TodoStatus::Pending),
    ]);
    let mut app = working_app();
    app.sessions.current.todos.items = todos;
    // A fresh completion timestamp keeps the done item visible (30s TTL).
    app.sessions
        .current
        .todos
        .completion_at
        .insert("done".into(), Instant::now());
    let out = render_text(&app, 80, 24);
    assert!(out.contains("✔"), "done glyph missing:\n{out}");
    assert!(out.contains("◻"), "pending glyph missing:\n{out}");
    assert!(
        !out.contains("… +"),
        "footer should be absent when nothing hidden:\n{out}"
    );
}

#[test]
fn test_empty_checklist_no_block() {
    let mut app = working_app();
    app.sessions.current.todos.items = Vec::new();
    let out = render_text(&app, 80, 24);
    assert!(
        !out.contains("◼"),
        "no active glyph expected on empty list:\n{out}"
    );
    assert!(
        !out.contains("tasks:"),
        "no tasks line expected on empty list:\n{out}"
    );
}

/// Once every task's completion window expires, the entire transcript block
/// clears instead of leaving a permanent gray completed-count summary.
#[test]
fn test_done_tasks_clear() {
    let mut app = working_app();
    app.sessions.current.todos.items = seeded_todos(&[
        ("first", TodoStatus::Completed),
        ("second", TodoStatus::Completed),
    ]);
    let now = Instant::now();
    let completed = now - Duration::from_secs(6);
    app.sessions
        .current
        .todos
        .completion_at
        .insert("first".into(), completed);
    app.sessions
        .current
        .todos
        .completion_at
        .insert("second".into(), completed);
    assert!(app.sessions.current.todos.prune(now));

    let out = render_text(&app, 80, 24);

    assert!(
        !out.contains("tasks ("),
        "completed block must clear:\n{out}"
    );
    assert!(
        !out.contains("completed"),
        "gray summary must clear:\n{out}"
    );
}

#[test]
fn test_done_stays_open() {
    let mut app = working_app();
    app.sessions.current.todos.items = seeded_todos(&[
        ("finished", TodoStatus::Completed),
        ("next", TodoStatus::Pending),
    ]);
    app.sessions
        .current
        .todos
        .completion_at
        .insert("finished".into(), Instant::now() - Duration::from_secs(31));

    let out = render_text(&app, 80, 24);

    assert!(out.contains("next"), "pending task remains:\n{out}");
    assert!(
        out.contains("finished"),
        "completed task remains while work is open:\n{out}"
    );
}

#[test]
fn test_status_shows_session_tasks() {
    let todos = seeded_todos(&[("run tests", TodoStatus::InProgress)]);
    let mut app = working_app();
    app.sessions.current.todos.items = todos;
    app.run_command(SlashCommand::Status);
    // /status opens a pane (the shared Pane template), not a transcript dump;
    // render the working surface to read the pane content.
    let out = render_text(&app, 80, 40);
    // The checklist is transcript content, not a /status row: it is on
    // screen in the transcript above the pane, and repeating it inside the
    // pane would put one list on two surfaces with two sets of counts to
    // keep in step.
    let lines: Vec<&str> = out.lines().collect();
    let tab = lines
        .iter()
        .position(|l| l.contains("[Status]"))
        .unwrap_or_else(|| panic!("no sub-tab header:\n{out}"));
    let hint = lines
        .iter()
        .position(|l| l.contains("switch tab"))
        .unwrap_or_else(|| panic!("no key hint:\n{out}"));
    let body: Vec<&str> = lines[tab..hint].to_vec();
    assert!(
        !body.iter().any(|l| l.contains("tasks (")),
        "the pane does not repeat the checklist:\n{out}"
    );
    assert!(
        out.contains("doing run tests"),
        "the transcript still shows it:\n{out}"
    );
}

#[test]
fn test_status_no_tasks_section() {
    let mut app = working_app();
    app.sessions.current.todos.items = Vec::new();
    app.run_command(SlashCommand::Status);
    // A taller terminal so the status pane admits the full field set.
    let out = render_text(&app, 80, 40);
    assert!(
        !out.contains("tasks:"),
        "no tasks section expected without a checklist:\n{out}"
    );
}

/// The header above the list and the footer under it count the same states. Two
/// vocabularies for one list make the reader stop and check whether the lines
/// describe the same thing, when the footer's only job is to say how many rows
/// the fold is hiding. Both lines use the header's words, in the header's order.
#[test]
fn test_footer_uses_header_words() {
    let todos = seeded_todos(&[
        ("setup", TodoStatus::Completed),
        ("doing", TodoStatus::InProgress),
        ("a", TodoStatus::Pending),
        ("b", TodoStatus::Pending),
        ("c", TodoStatus::Pending),
        ("d", TodoStatus::Pending),
    ]);
    let mut app = working_app();
    app.sessions.current.todos.items = todos;
    app.sessions
        .current
        .todos
        .completion_at
        .insert("setup".into(), Instant::now());
    let out = render_text(&app, 80, 24);
    let header = out
        .lines()
        .find(|l| l.contains("tasks ("))
        .unwrap_or_else(|| panic!("no header:\n{out}"));
    let footer = out
        .lines()
        .find(|l| l.contains("hidden:"))
        .unwrap_or_else(|| panic!("no footer:\n{out}"));
    // Every state the footer names is one the header names, so the two lines
    // are read with one vocabulary.
    for word in ["done", "in progress", "paused", "open"] {
        let named_in_footer =
            footer.contains(&format!(" {word}")) || footer.contains(&format!("+{word}"));
        assert!(
            !named_in_footer || header.contains(word),
            "the footer names {word:?} but the header does not: {header}"
        );
    }
    // The words the old footer used are gone: one list, one vocabulary.
    assert!(
        !footer.contains("pending"),
        "no second word for open: {footer}"
    );
    assert!(
        !footer.contains("completed"),
        "no second word for done: {footer}"
    );
    // A state nobody is in is not printed as a zero.
    assert!(
        !header.contains("0 paused"),
        "a zero state is left out: {header}"
    );
}

/// The expanded header names the way back. The checklist can fill the screen,
/// and the row that opened it is the one the reader looks at to close it; a
/// header that only counted states leaves that reader hunting.
#[test]
fn test_expanded_header_hints() {
    let mut app = working_app();
    app.sessions.current.todos.items = seeded_todos(&[
        ("one", TodoStatus::Completed),
        ("two", TodoStatus::InProgress),
        ("three", TodoStatus::Pending),
    ]);
    app.sessions.current.todos.expanded = true;
    let out = render_text(&app, 100, 30);
    let header = out
        .lines()
        .find(|l| l.contains("tasks ("))
        .unwrap_or_else(|| panic!("no header:\n{out}"));
    assert!(
        header.contains("ctrl+o to collapse"),
        "the expanded header names the chord that closes it: {header}"
    );
}

/// The collapsed header does not promise a collapse: with the list shut the
/// chord is what opens it, and naming the opposite would be wrong.
#[test]
fn test_collapsed_header_quiet() {
    let mut app = working_app();
    app.sessions.current.todos.items =
        seeded_todos(&[("one", TodoStatus::Completed), ("two", TodoStatus::Pending)]);
    let out = render_text(&app, 100, 30);
    let header = out
        .lines()
        .find(|l| l.contains("tasks ("))
        .unwrap_or_else(|| panic!("no header:\n{out}"));
    assert!(
        !header.contains("collapse"),
        "a shut list offers no collapse: {header}"
    );
}
