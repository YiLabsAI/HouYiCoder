//! Pointer reports routed to the surface that owns the gesture.
//!
//! The terminal reports wheel, click, drag, and release over the whole window;
//! this module decides which surface a report lands on, in the order the
//! screen stacks them, and answers whether the report changed anything a
//! frame can show.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use houyicoder_protocol::frontend::SlashCommand;
use ratatui::layout::Rect;
use std::cmp;

use crate::selection::region::{
    ApprovalSelection, PaneRegion, Region, StatusRegion, TranscriptRegion, clear_stale_click,
    edge_scroll_intent,
};
use crate::state::{App, Pane};
use crate::view::working::fleet_strip::{FleetClick, click_route};

/// Route wheel, click, and drag gestures to the active in-app surface.
/// Releasing a selection copies it through the configured clipboard path.
///
/// The answer says whether the gesture changed anything the frame can show.
/// Motion with no drag in progress, a drag repeating the cell it last landed
/// on, and a wheel already at the end of its range changed nothing and answer
/// false; a press, a release, and a wheel step answer true. The terminal
/// reports motion for every cell the pointer crosses, so answering a redraw
/// for each one would repaint the full transcript at the frame cap while the
/// user is doing nothing.
pub(crate) fn handle_mouse(app: &mut App, m: MouseEvent) -> bool {
    tracing::debug!(kind = ?m.kind, col = m.column, row = m.row, "mouse event");
    let mut gestures = PointerGestures { app };
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => gestures.press(&m),
        MouseEventKind::Drag(MouseButton::Left) => gestures.drag_to(&m),
        MouseEventKind::Moved => gestures.close_lost_drags(),
        MouseEventKind::Up(MouseButton::Left) => gestures.release(),
        MouseEventKind::ScrollUp => gestures.wheel(true),
        MouseEventKind::ScrollDown => gestures.wheel(false),
        _ => false,
    }
}

/// The pointer gesture a terminal report names, routed to the surface that
/// owns it. One value per report, borrowing the app for the gesture.
struct PointerGestures<'a> {
    app: &'a mut App,
}

impl PointerGestures<'_> {
    /// A press, in hit-test order. The jump-to-bottom label overlays the
    /// transcript, so it owns the first hit-test when visible, and the footer
    /// queue strip comes next; the surfaces stacked over the transcript then
    /// take the press before the transcript does.
    fn press(&mut self, m: &MouseEvent) -> bool {
        if self.jump_label(m) {
            return true;
        }
        if let Some(changed) = self.queue_strip(m) {
            return changed;
        }
        self.press_region(m);
        true
    }

    /// A press on the jump-to-bottom label returns the view to the tail.
    fn jump_label(&mut self, m: &MouseEvent) -> bool {
        let rect = self.app.ui.frame.layout.jump_to_bottom_rect.get();
        if rect.width == 0 || rect.height == 0 || !in_rect(rect, m.column, m.row) {
            return false;
        }
        self.app.scroll_transcript_follow_tail();
        true
    }

    /// A press on the footer queue strip. A previewed item is recalled into
    /// the input box, and the count row opens the queue pane without
    /// recalling anything. None means the press landed elsewhere; Some(false)
    /// means it landed there and changed nothing.
    fn queue_strip(&mut self, m: &MouseEvent) -> Option<bool> {
        let rect = self.app.ui.frame.layout.queue_rect.get();
        if rect.width == 0 || rect.height == 0 || !in_rect(rect, m.column, m.row) {
            return None;
        }
        let count = self
            .app
            .ui
            .input
            .queue
            .iter()
            .filter(|item| !item.display().is_empty())
            .count();
        if count == 0 {
            return Some(false);
        }
        if rect.height <= 1 {
            self.app.ui.navigation.pane = Pane::Queue;
            self.app.ui.input.queue.select_first();
            return Some(true);
        }
        let row = (m.row - rect.y) as usize;
        let shown = if count > 2 { 1 } else { cmp::min(count, 2) };
        if row < shown {
            let index = self
                .app
                .ui
                .input
                .queue
                .iter()
                .enumerate()
                .filter(|(_, item)| !item.display().is_empty())
                .nth(row)
                .map(|(index, _)| index);
            let Some(index) = index else {
                return Some(false);
            };
            let Some(item) = self.app.remove_pending_at(index) else {
                return Some(false);
            };
            self.app
                .ui
                .input
                .merge_recalled_text(item.display().to_string());
        } else {
            self.app.ui.navigation.pane = Pane::Queue;
            self.app.ui.input.queue.select_first();
        }
        Some(true)
    }

    /// Route a press to the surfaces stacked over the transcript, then to the
    /// transcript itself. A press outside all of them drops the selections it
    /// cannot reach.
    fn press_region(&mut self, m: &MouseEvent) {
        let prect = self.app.ui.frame.layout.pane_rect.get();
        if prect.width > 0 && prect.height > 0 && in_rect(prect, m.column, m.row) {
            let intent = PaneRegion {
                view: self.app.region_view(),
            }
            .handle_down(m.column, m.row);
            self.app.apply_region_intent(intent);
            return;
        }
        let srect = self.app.ui.frame.layout.status_rect.get();
        if srect.width > 0 && srect.height > 0 && in_rect(srect, m.column, m.row) {
            let intent = StatusRegion {
                view: self.app.region_view(),
            }
            .handle_down(m.column, m.row);
            self.app.apply_region_intent(intent);
            return;
        }
        let arect = self.app.ui.frame.layout.approval_rect.get();
        if arect.width > 0 && arect.height > 0 && in_rect(arect, m.column, m.row) {
            let intent = ApprovalSelection {
                view: self.app.region_view(),
            }
            .handle_down(m.column, m.row);
            self.app.apply_region_intent(intent);
            return;
        }
        if self.press_fleet(m) {
            return;
        }
        if self.app.press_in_composer(m) {
            self.app.composer_press(m);
            return;
        }
        let rect = self.app.ui.frame.layout.transcript_rect.get();
        if in_rect(rect, m.column, m.row) {
            // Byte-window browsing does not share transcript selection
            // coordinates, so clicks do not start a drag.
            if !self.app.window_browsing() {
                let intent = TranscriptRegion {
                    view: self.app.region_view(),
                }
                .handle_down(m.column, m.row);
                self.app.apply_region_intent(intent);
            }
        } else {
            self.app.ui.selection.transcript.clear();
            self.app.ui.selection.pane.clear();
            self.app.ui.selection.status.clear();
            self.app.ui.selection.approval.clear();
        }
    }

    /// A press on the fleet strip routes through the row map before any state
    /// changes, so a click that lands on no row costs nothing.
    fn press_fleet(&mut self, m: &MouseEvent) -> bool {
        let rect = self.app.ui.frame.layout.fleet_rect.get();
        if rect.width == 0 || rect.height == 0 || !in_rect(rect, m.column, m.row) {
            return false;
        }
        let entries = &self.app.sessions.current.capabilities.fleet.entries;
        let selected = self.app.ui.fleet.selected();
        let route = click_route(entries, selected, rect.height, (m.row - rect.y) as usize);
        match route {
            FleetClick::OpenAgentsPane => self.app.run_command(SlashCommand::Agents),
            FleetClick::Select(index) => self.app.ui.fleet.set_selected(Some(index)),
            FleetClick::Drill(sid) => {
                self.app.enter_teammate_view_for_sid(&sid, true);
            }
        }
        true
    }

    /// Extend the active drag to the pointer's cell.
    ///
    /// A drag that repeats the cell the last report landed on moves no end of
    /// the selection, and no surface is dragging when the terminal reports the
    /// gesture after its release was lost.
    fn drag_to(&mut self, m: &MouseEvent) -> bool {
        match dragging_cursor(self.app) {
            None => return false,
            Some(cell) if cell == (m.column, m.row) => return false,
            Some(_) => {}
        }
        if self.app.ui.selection.input.is_dragging {
            self.app.composer_drag(m);
        } else if self.app.ui.selection.pane.is_dragging {
            let intent = PaneRegion {
                view: self.app.region_view(),
            }
            .handle_drag(m.column, m.row);
            self.app.apply_region_intent(intent);
        } else if self.app.ui.selection.status.is_dragging {
            let intent = StatusRegion {
                view: self.app.region_view(),
            }
            .handle_drag(m.column, m.row);
            self.app.apply_region_intent(intent);
        } else if self.app.ui.selection.approval.is_dragging {
            let intent = ApprovalSelection {
                view: self.app.region_view(),
            }
            .handle_drag(m.column, m.row);
            self.app.apply_region_intent(intent);
        } else if self.app.ui.selection.transcript.is_dragging {
            // Edge auto-scroll first: the drag body maps the focus through
            // the post-scroll offset, so the row it lands on is the one the
            // next render shows under the cursor.
            let rect = self.app.ui.frame.layout.transcript_rect.get();
            self.app
                .apply_region_intent(edge_scroll_intent(rect, m.row));
            let intent = TranscriptRegion {
                view: self.app.region_view(),
            }
            .handle_drag(m.column, m.row);
            self.app.apply_region_intent(intent);
        }
        true
    }

    /// Close every drag whose release never arrived. Motion without a pressed
    /// button is the only signal for it, and several surfaces may still carry
    /// stale drag state from a release that landed outside the window.
    fn close_lost_drags(&mut self) -> bool {
        let mut changed = false;
        if self.app.ui.selection.input.is_dragging {
            self.app.composer_moved();
            changed = true;
        }
        if self.app.ui.selection.pane.is_dragging {
            let intent = PaneRegion {
                view: self.app.region_view(),
            }
            .handle_moved();
            self.app.apply_region_intent(intent);
            changed = true;
        }
        if self.app.ui.selection.status.is_dragging {
            let intent = StatusRegion {
                view: self.app.region_view(),
            }
            .handle_moved();
            self.app.apply_region_intent(intent);
            changed = true;
        }
        if self.app.ui.selection.approval.is_dragging {
            let intent = ApprovalSelection {
                view: self.app.region_view(),
            }
            .handle_moved();
            self.app.apply_region_intent(intent);
            changed = true;
        }
        if self.app.ui.selection.transcript.is_dragging {
            let intent = TranscriptRegion {
                view: self.app.region_view(),
            }
            .handle_moved();
            self.app.apply_region_intent(intent);
            changed = true;
        }
        changed
    }

    /// Finish the gesture under the pointer and copy what it covered. A release
    /// with no drag in progress changes nothing.
    fn release(&mut self) -> bool {
        if self.app.ui.selection.input.is_dragging {
            self.app.composer_release();
            return true;
        }
        if self.app.ui.selection.pane.is_dragging {
            let intent = PaneRegion {
                view: self.app.region_view(),
            }
            .handle_up();
            self.app.apply_region_intent(intent);
            return true;
        }
        if self.app.ui.selection.status.is_dragging {
            let intent = StatusRegion {
                view: self.app.region_view(),
            }
            .handle_up();
            self.app.apply_region_intent(intent);
            return true;
        }
        if self.app.ui.selection.approval.is_dragging {
            let intent = ApprovalSelection {
                view: self.app.region_view(),
            }
            .handle_up();
            self.app.apply_region_intent(intent);
            return true;
        }
        if !self.app.ui.selection.transcript.is_dragging {
            return false;
        }
        let intent = TranscriptRegion {
            view: self.app.region_view(),
        }
        .handle_up();
        self.app.apply_region_intent(intent);
        true
    }

    /// One wheel step over the transcript or the byte window, answering
    /// whether the view actually moved. A wheel already at the top or bottom
    /// has nowhere to go, but erasing a stale click still changes the screen.
    fn wheel(&mut self, up: bool) -> bool {
        let cleared = clear_stale_click(&mut self.app.ui.selection.transcript)
            | clear_stale_click(&mut self.app.ui.selection.pane);
        let before = self.app.active_scroll().top_offset();
        if self.app.window_browsing() {
            if up {
                self.app.window_line_up(3);
            } else {
                self.app.window_line_down(3);
            }
        } else if up {
            self.app.scroll_transcript_line_up(3);
        } else {
            self.app.scroll_transcript_line_down(3);
        }
        cleared || self.app.active_scroll().top_offset() != before
    }
}

/// The screen cell the active drag last landed on, or None when no surface is
/// dragging. The terminal reports motion per cell, so a drag that repeats the
/// last cell asks for no change.
fn dragging_cursor(app: &App) -> Option<(u16, u16)> {
    let selection = &app.ui.selection;
    if selection.input.is_dragging {
        selection.input.cursor
    } else if selection.pane.is_dragging {
        selection.pane.cursor
    } else if selection.status.is_dragging {
        selection.status.cursor
    } else if selection.approval.is_dragging {
        selection.approval.cursor
    } else if selection.transcript.is_dragging {
        selection.transcript.cursor
    } else {
        None
    }
}

/// True when (x, y) is inside the rect (exclusive of the far edge).
fn in_rect(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}
