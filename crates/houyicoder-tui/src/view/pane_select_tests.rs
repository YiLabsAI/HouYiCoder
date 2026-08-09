//! Discriminating tests for slash-pane drag-select + copy (bug #62). The
//! transcript drag-select path is covered by drag_select_bug_tests; these pin
//! that the slash-command panes (/memory here, shared template with
//! /permissions and /search) publish their rows to the pane selection
//! surface and a drag in the pane copies the pane text — not the transcript
//! row stash the prior bug surfaced. Follows the drag_select_bug_tests style
//! (RecordingClipboard + real handle_mouse over a real render).

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::style::Color;
use std::sync::{Arc, Mutex};

use crate::composition;
use crate::selection::RecordingClipboard;
use crate::state::Screen;
use crate::test_support::{render_buffer, render_text};
use houyicoder_protocol::frontend::SlashCommand;

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn memory_app() -> (crate::state::App, Arc<Mutex<Vec<String>>>) {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.run_command(SlashCommand::Memory);
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    app.clipboard = Arc::new(RecordingClipboard {
        captured: captured.clone(),
    });
    let _rendered = render_text(&app, 100, 28);
    (app, captured)
}

/// The index (into last_pane_rows) of the pane row carrying the needle, plus
/// its screen y. The memory pane stashes one (tag, text) per rendered row via
/// stash_pane_rows, so the index maps straight to a screen row offset from
/// pane_rect.y.
fn pane_row(app: &crate::state::App, needle: &str) -> (usize, u16) {
    let prect = app.pane_rect.get();
    let rows = app.last_pane_rows.borrow();
    let ri = rows
        .iter()
        .position(|(_, s)| s.contains(needle))
        .expect("needle pane row present");
    (ri, prect.y + ri as u16)
}

/// A Down → Drag → Up over a /memory entry row writes that row's text to the
/// clipboard. Pins the whole pane selection surface: PaneSurface::parts +
/// handle_down/drag/up + extract_text + the copy path, and that the pane row
/// stash (not the transcript stash) is the source.
#[test]
fn test_drag_copies_pane_row() {
    let (mut app, captured) = memory_app();
    let (ri, y) = pane_row(&app, "build-gate");
    let prect = app.pane_rect.get();
    let x0 = prect.x + 2;
    // Down starts the drag; drag right; up copies + clears (pane does not
    // persist the highlight).
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), x0, y),
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Drag(MouseButton::Left), x0 + 30, y),
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Up(MouseButton::Left), x0 + 30, y),
    );
    let copied = captured.lock().expect("captured").join("");
    assert!(
        copied.contains("build-gate"),
        "clipboard should hold the pane row text, got: {copied:?}"
    );
    // The pane range is cleared on release — no stale highlight.
    assert!(
        !app.pane_selection.is_dragging,
        "pane selection cleared on release"
    );
    let _ = ri; // row index used only for the screen-y derivation
}

/// Mid-drag the dragged pane cells carry the selection background, so the user
/// sees what they are about to copy. Pins the paint_overlay pane pass
/// (screen-space, scroll_top 0) through apply_selection_overlay.
#[test]
fn test_pane_drag_paints_cells() {
    let (mut app, _captured) = memory_app();
    let (_, y) = pane_row(&app, "build-gate");
    let prect = app.pane_rect.get();
    let x0 = prect.x + 2;
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), x0, y),
    );
    crate::app::handle_mouse(
        &mut app,
        mouse(MouseEventKind::Drag(MouseButton::Left), x0 + 6, y),
    );
    let buf = render_buffer(&app, 100, 28);
    // A cell inside the dragged range on the entry row is selection-bg.
    let cell = buf.cell((x0 + 2, y)).expect("cell inside pane drag range");
    assert_eq!(
        cell.bg,
        Color::Indexed(24),
        "dragged pane cell must carry the selection bg"
    );
}
