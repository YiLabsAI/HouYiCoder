//! Drag-select and copy inside a slash-command pane. The transcript path is
//! covered by drag_select_tests; these pin that a pane (/memory here, the
//! shared template for /permissions and /search) publishes its own rows and
//! that a drag in it copies the pane text rather than the transcript's. The
//! fixtures follow drag_select_tests: a recording clipboard over a real
//! handle_mouse on a real render.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::style::Color;
use std::sync::{Arc, Mutex};

use crate::app::handle_mouse;
use crate::composition;
use crate::selection::RecordingClipboard;
use crate::state::Screen;
use crate::test_harness::{render_buffer, render_text, working_app};
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
    app.ui.navigation.screen = Screen::Working;
    app.run_command(SlashCommand::Memory);
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    app.ui.clipboard = Arc::new(RecordingClipboard {
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
    let prect = app.ui.frame.layout.pane_rect.get();
    let rows = app.ui.frame.layout.last_pane_rows.borrow();
    let ri = rows
        .iter()
        .position(|(_, s)| s.contains(needle))
        .expect("needle pane row present");
    (ri, prect.y + ri as u16)
}

/// A Down → Drag → Up over a /memory entry row writes that row's text to the
/// clipboard. Pins the whole pane selection path: PaneRegion::parts +
/// handle_down/drag/up + extract_text + the copy path, and that the pane row
/// stash (not the transcript stash) is the source.
#[test]
fn test_drag_copies_pane_row() {
    let (mut app, captured) = memory_app();
    let (ri, y) = pane_row(&app, "build-gate");
    let prect = app.ui.frame.layout.pane_rect.get();
    let x0 = prect.x + 2;
    // Down starts the drag; drag right; up copies + clears (pane does not
    // persist the highlight).
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), x0, y),
    );
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Drag(MouseButton::Left), x0 + 30, y),
    );
    handle_mouse(
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
        !app.ui.selection.pane.is_dragging,
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
    let prect = app.ui.frame.layout.pane_rect.get();
    let x0 = prect.x + 2;
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), x0, y),
    );
    handle_mouse(
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

/// The status bar is its own selection region: a press and a drag inside it
/// route there rather than to the transcript underneath. A motion with no
/// button then closes every drag that still carries state, which is the
/// lost-release recovery the router runs for each region.
#[test]
fn test_status_drag_and_moved() {
    // The memory pane retracts the status bar, so this uses the bare working
    // screen: the region under test is the bar itself.
    let mut app = working_app();
    let _rendered = render_text(&app, 100, 28);
    let srect = app.ui.frame.layout.status_rect.get();
    assert!(srect.height > 0, "the status bar rendered a rect");

    handle_mouse(
        &mut app,
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            srect.x + 2,
            srect.y,
        ),
    );
    handle_mouse(
        &mut app,
        mouse(
            MouseEventKind::Drag(MouseButton::Left),
            srect.x + 12,
            srect.y,
        ),
    );
    assert!(
        app.ui.selection.status.is_dragging,
        "a drag inside the status bar belongs to its region"
    );

    // Motion without a pressed button closes the drag whose release was lost,
    // for every region still carrying one.
    app.ui.selection.pane.is_dragging = true;
    app.ui.selection.approval.is_dragging = true;
    handle_mouse(&mut app, mouse(MouseEventKind::Moved, srect.x, srect.y));

    assert!(!app.ui.selection.status.is_dragging, "status drag closed");
    assert!(!app.ui.selection.pane.is_dragging, "pane drag closed");
    assert!(
        !app.ui.selection.approval.is_dragging,
        "approval drag closed"
    );
}

/// Releasing a drag in the status bar copies the text it selected and says so.
/// The notice is the router's job under the intent model, which is what makes
/// every region report a copy the same way instead of only the transcript.
#[test]
fn test_status_release_copies() {
    let mut app = working_app();
    let _rendered = render_text(&app, 100, 28);
    let srect = app.ui.frame.layout.status_rect.get();

    handle_mouse(
        &mut app,
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            srect.x + 2,
            srect.y,
        ),
    );
    handle_mouse(
        &mut app,
        mouse(
            MouseEventKind::Drag(MouseButton::Left),
            srect.x + 20,
            srect.y,
        ),
    );
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Up(MouseButton::Left), srect.x + 20, srect.y),
    );

    let out = render_text(&app, 100, 28);
    assert!(
        out.contains("copied"),
        "the release reports the copy it made:\n{out}"
    );
}
