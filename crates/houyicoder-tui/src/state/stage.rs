//! Stage transitions and the two manual viewport exits (Scroll and Focus) on
//! App. The stage, its history, and the spec strip step are one value and move
//! inside Workflow; what stays here is the half that crosses owners — syncing
//! the viewport the new stage implies.

use crate::state::{App, Stage, ViewportMode};

impl App {
    /// Transition to a new stage, pushing the previous one onto the history
    /// stack so /rewind can return to it. Also updates the spec strip step and
    /// syncs the viewport to the new stage (implement and verify fold into
    /// Focus).
    pub fn set_stage(&mut self, stage: Stage) {
        self.workflow.enter(stage);
        self.ui.sync_viewport_to_stage(stage);
    }

    /// Leave Scroll mode and restore the prior viewport. Only switches the
    /// viewport: it does not return to the tail, so typing or opening a search
    /// from scroll mode keeps the view. Esc and End add follow_tail at their
    /// call sites for the tail-return behavior.
    pub fn exit_scroll(&mut self) {
        self.ui.navigation.viewport = self.ui.navigation.prev_viewport;
    }

    /// Fold Focus into Working (user pressed Esc in Focus). Stage unchanged.
    pub fn fold_to_working(&mut self) {
        self.ui.navigation.viewport = ViewportMode::Working;
    }

    /// Pop the most recent stage off the history stack and restore it. Returns
    /// the restored stage when there was something to rewind to. Also syncs the
    /// viewport so rewinding from Focus back to design unfolds to Working.
    pub fn rewind_stage(&mut self) -> Option<Stage> {
        let previous = self.workflow.rewind()?;
        self.ui.sync_viewport_to_stage(previous);
        Some(previous)
    }
}
