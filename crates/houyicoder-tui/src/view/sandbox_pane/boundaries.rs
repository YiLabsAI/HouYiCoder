//! The sandbox tab: what confines the session, and the reason when the host
//! could not set a fence up.

use houyicoder_protocol::frontend::checkpoint::FenceState;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::TABS;
use crate::state::App;
use crate::view::navigation::{block_with_keys, tab_header};

/// What confines the session. A fence the host could not set up states its
/// reason rather than reading as a healthy boundary, and a session with no
/// fence says so on the row where the user looks for the answer.
pub(super) fn draw_boundaries(f: &mut Frame, area: Rect, app: &App) {
    let pane = &app.ui.sandbox_pane;
    let mut lines = vec![tab_header(pane.tab.get(), &TABS)];
    match pane.boundaries() {
        Some(view) => {
            let (label, color) = status_label(&view.fence);
            lines.push(Line::from(vec![
                Span::raw("  Status:    "),
                Span::styled(label, Style::new().fg(color)),
            ]));
            lines.push(Line::from(format!("  Workspace: {}", view.workspace_root)));
            let mut roots = view.writable_roots.iter();
            if let Some(first) = roots.next() {
                lines.push(Line::from(format!("  Writable:  {first}")));
                if let Some(scratch) = &view.scratch_root {
                    lines.push(Line::from(format!(
                        "             {scratch} (session TMPDIR)"
                    )));
                }
                for root in roots {
                    lines.push(Line::from(format!("             {root}")));
                }
            } else if let Some(scratch) = &view.scratch_root {
                lines.push(Line::from(format!(
                    "  Writable:  {scratch} (session TMPDIR)"
                )));
            } else {
                lines.push(Line::from("  Writable:  nothing outside the workspace"));
            }
        }
        None if pane.reading_boundaries() => {
            lines.push(Line::from(
                "  Reading the fence and the roots from the host…",
            ));
        }
        None => lines.push(Line::from(
            "  The host has not answered; reopening the pane reads again",
        )),
    }
    if let Some(notice) = pane.notice() {
        lines.push(Line::from(Span::styled(
            notice,
            Style::new().fg(Color::Yellow),
        )));
    }
    block_with_keys(
        f,
        area,
        lines,
        &[("Tab/Left/Right", "switch tab"), ("Esc", "close")],
        &pane.scroll,
    );
}

/// The fence's state in the words the user acts on. A fence that failed says
/// why, because the list of writable roots below then describes a boundary
/// nothing enforces.
fn status_label(fence: &FenceState) -> (String, Color) {
    match fence {
        FenceState::Enforced => (
            "Enforced (writes outside the roots below are refused)".to_string(),
            Color::Green,
        ),
        FenceState::NotEnforced => (
            "Not enforced (the session can write anywhere its user can)".to_string(),
            Color::Yellow,
        ),
        FenceState::Failed(reason) => (format!("Could not be set up: {reason}"), Color::Red),
        FenceState::Unavailable => (
            "Not offered on this platform (writes follow its permission)".to_string(),
            Color::Yellow,
        ),
    }
}
