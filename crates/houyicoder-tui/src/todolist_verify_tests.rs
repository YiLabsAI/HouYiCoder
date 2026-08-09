//! Flow-level verification for the checklist render and the /status command.
//! These drive the real view::draw -> working::draw_transcript pipeline (the
//! checklist renders as selectable rows at the transcript tail) and the real
//! command dispatch (not pure-fn helpers), with a checklist view model
//! populated the way the wire accumulator would leave it.

use houyicoder_protocol::frontend::SlashCommand;

use crate::composition;
use crate::test_support::render_text;
use crate::todo_view::{TodoStatus, TodoView};

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

fn working_app() -> crate::state::App {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
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
    app.todos_cache = todos;
    // A fresh completion timestamp keeps the done item visible (30s TTL).
    app.todo_completion_at
        .insert("setup".into(), std::time::Instant::now());
    let out = render_text(&app, 80, 24);
    // The active item uses the cyan glyph and the active-form label.
    assert!(out.contains("◼"), "active glyph missing:\n{out}");
    assert!(
        out.contains("doing run tests"),
        "active-form missing:\n{out}"
    );
    // The recently completed item shows the done glyph.
    assert!(out.contains("✔"), "done glyph missing:\n{out}");
    // Two pending items are hidden beyond the three visible slots -> footer.
    assert!(out.contains("… +"), "hidden footer missing:\n{out}");
    assert!(
        out.contains("pending"),
        "pending count in footer missing:\n{out}"
    );
}

#[test]
fn test_collapsed_no_footer_visible() {
    let todos = seeded_todos(&[
        ("done", TodoStatus::Completed),
        ("next", TodoStatus::Pending),
    ]);
    let mut app = working_app();
    app.todos_cache = todos;
    // A fresh completion timestamp keeps the done item visible (30s TTL).
    app.todo_completion_at
        .insert("done".into(), std::time::Instant::now());
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
    app.todos_cache = Vec::new();
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

#[test]
fn test_status_shows_session_tasks() {
    let todos = seeded_todos(&[("run tests", TodoStatus::InProgress)]);
    let mut app = working_app();
    app.todos_cache = todos;
    app.run_command(SlashCommand::Status);
    // /status opens a pane (the shared Pane template), not a transcript dump;
    // render the working surface to read the pane content. A tall-enough
    // terminal so the pane's area/2 cap admits the tasks section.
    let out = render_text(&app, 80, 40);
    // Todos stay on the Status tab; tokens/wall-duration moved to Usage.
    assert!(out.contains("tasks: 1"), "tasks count missing:\n{out}");
    assert!(
        out.contains("in progress, 0 open"),
        "tasks breakdown missing:\n{out}"
    );
    assert!(
        out.contains("doing run tests"),
        "task item line missing:\n{out}"
    );
}

#[test]
fn test_status_no_tasks_section() {
    let mut app = working_app();
    app.todos_cache = Vec::new();
    app.run_command(SlashCommand::Status);
    // A taller terminal so the status pane admits the full field set.
    let out = render_text(&app, 80, 40);
    assert!(
        !out.contains("tasks:"),
        "no tasks section expected without a checklist:\n{out}"
    );
}
