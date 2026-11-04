//! The five drag selections App holds, one per interactable region: the
//! transcript's main selection, plus the status, pane, and approval-card
//! selections and the composer. Each is a full Selection in its own coordinate space so a drag
//! on one region never collides with another; the holder groups the four
//! same-typed values so a region's selection travels beside its siblings
//! rather than as five loose App fields.

use crate::selection::Selection;

/// The five per-region drag selections, one Selection per region in its own
/// coordinate space.
#[derive(Default, Clone, Debug)]
pub(crate) struct Selections {
    /// The transcript drag-select: the user's in-app text selection.
    pub(crate) transcript: Selection,
    /// The status-bar row selection.
    pub(crate) status: Selection,
    /// The pane (slash-command and slide-over) selection.
    pub(crate) pane: Selection,
    /// The approval-card selection.
    pub(crate) approval: Selection,
    /// The composer's selection: the draft the user dragged over.
    pub(crate) input: Selection,
}

impl Selections {
    /// Clear every region's drag or held selection.
    pub(crate) fn clear(&mut self) {
        self.transcript.clear();
        self.status.clear();
        self.pane.clear();
        self.approval.clear();
        self.input.clear();
    }
}
