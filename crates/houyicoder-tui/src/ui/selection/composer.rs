//! The composer as a selection surface.
//!
//! The draft is the one editable surface, so a drag over it selects text the
//! way an editor does and the highlight survives the release. Keeping it here
//! leaves the transcript and pane surfaces in the shared region module.

use crossterm::event::MouseEvent;
use ratatui::buffer::Buffer;

use super::region::{
    ApprovalSelection, PaneRegion, Region, RegionParts, RegionView, StatusRegion, TranscriptRegion,
    paint_overlay,
};
use crate::state::App;

/// The composer selection surface.
pub(crate) struct InputRegion<'a> {
    pub(crate) view: RegionView<'a>,
}

impl Region for InputRegion<'_> {
    fn parts(&mut self) -> RegionParts<'_> {
        let layout = &self.view.frame.layout;
        let rows = layout.last_input_rows.borrow();
        RegionParts {
            sel: &mut self.view.selections.input,
            rows,
            rect: layout.input_rect.get(),
            clipboard: self.view.clipboard,
        }
    }
    fn to_content(&self, x: u16, y: u16) -> (u16, usize) {
        let rect = self.view.frame.layout.input_rect.get();
        (
            x.saturating_sub(rect.x).min(rect.width.saturating_sub(1)),
            (y.saturating_sub(rect.y)) as usize,
        )
    }
    /// The composer keeps its highlight after the release: it is an editor,
    /// so the range the user dragged stays visible to read or copy again.
    fn persist(&self) -> bool {
        true
    }
    fn is_dragging(&self) -> bool {
        self.view.selections.input.is_dragging
    }
}

impl App {
    /// Whether a press landed inside the composer.
    pub(crate) fn press_in_composer(&self, m: &MouseEvent) -> bool {
        let rect = self.ui.frame.layout.input_rect.get();
        rect.width > 0
            && rect.height > 0
            && m.column >= rect.x
            && m.column < rect.x + rect.width
            && m.row >= rect.y
            && m.row < rect.y + rect.height
    }

    /// Start a selection at the press and put the caret where the pointer landed.
    pub(crate) fn composer_press(&mut self, m: &MouseEvent) {
        self.ui.selection.clear();
        let intent = InputRegion {
            view: self.region_view(),
        }
        .handle_down(m.column, m.row);
        self.apply_region_intent(intent);
        let rect = self.ui.frame.layout.input_rect.get();
        let cols = self.ui.frame.feedback.terminal_cols.get();
        self.ui.input.editor.move_to_row_col(
            cols,
            (m.row - rect.y) as usize,
            (m.column - rect.x) as usize,
        );
    }

    /// Extend the composer's selection while the button is held.
    pub(crate) fn composer_drag(&mut self, m: &MouseEvent) {
        let intent = InputRegion {
            view: self.region_view(),
        }
        .handle_drag(m.column, m.row);
        self.apply_region_intent(intent);
    }

    /// Finish the composer's selection and copy what it covers.
    pub(crate) fn composer_release(&mut self) {
        let intent = InputRegion {
            view: self.region_view(),
        }
        .handle_up();
        self.apply_region_intent(intent);
    }

    /// Close a composer drag whose release never arrived.
    pub(crate) fn composer_moved(&mut self) {
        let intent = InputRegion {
            view: self.region_view(),
        }
        .handle_moved();
        self.apply_region_intent(intent);
    }
}

impl App {
    /// Copy the composer's held selection without dropping it.
    pub(crate) fn copy_composer_selection(&mut self) {
        let intent = InputRegion {
            view: self.region_view(),
        }
        .copy_current();
        self.apply_region_intent(intent);
    }
}

/// Paint the composer's held selection over the draft.
pub(crate) fn paint(buf: &mut Buffer, app: &App) {
    let rows = app.ui.frame.layout.last_input_rows.borrow();
    paint_overlay(
        buf,
        app.ui.frame.layout.input_rect.get(),
        &rows,
        &app.ui.selection.input,
        0,
    );
}

impl App {
    /// Copy whichever surface holds a selection. Returns false when none does.
    ///
    /// Whichever surface the user selected is what they mean to copy, and the
    /// composer is one of them: a draft it highlighted is text like any other.
    pub(crate) fn copy_active_selection(&mut self) -> bool {
        if self.ui.selection.input.has_selection() {
            self.copy_composer_selection();
        } else if self.ui.selection.pane.has_selection() {
            let intent = PaneRegion {
                view: self.region_view(),
            }
            .copy_current();
            self.apply_region_intent(intent);
        } else if self.ui.selection.status.has_selection() {
            let intent = StatusRegion {
                view: self.region_view(),
            }
            .copy_current();
            self.apply_region_intent(intent);
        } else if self.ui.selection.approval.has_selection() {
            let intent = ApprovalSelection {
                view: self.region_view(),
            }
            .copy_current();
            self.apply_region_intent(intent);
        } else if self.ui.selection.transcript.has_selection() {
            let intent = TranscriptRegion {
                view: self.region_view(),
            }
            .copy_current();
            self.apply_region_intent(intent);
        } else {
            return false;
        }
        true
    }
}
