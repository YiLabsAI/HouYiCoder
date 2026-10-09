//! Guided-workflow stage and viewport state on App: stage transitions, the
//! viewport sync that follows them, and the plan-mode toggle. Split from App
//! so the workflow state machine is one cohesive unit rather than scattered
//! App methods.

use crate::state::{App, Stage, ViewportMode};

impl App {
    /// Toggle plan mode on or off.
    pub fn toggle_plan(&mut self) {
        self.status.plan_mode = !self.status.plan_mode;
    }

    /// Transition to a new stage, pushing the previous one onto the history
    /// stack so /rewind can return to it. Also updates the spec strip step and
    /// syncs the viewport to the new stage (implement and verify fold into
    /// Focus).
    pub fn set_stage(&mut self, stage: Stage) {
        self.workflow.stage_history.push(self.workflow.stage);
        self.workflow.stage = stage;
        self.workflow.spec_ctx.step = stage.label().to_string();
        self.sync_viewport_to_stage();
    }

    /// Auto-set the viewport from the current stage. Implement and verify fold
    /// into Focus; idle, design, and done unfold to Working. Also closes any
    /// open palette or search when leaving Working so the inline-only layout
    /// cells do not linger.
    pub fn sync_viewport_to_stage(&mut self) {
        let next = ViewportMode::for_stage(self.workflow.stage);
        if next != ViewportMode::Working {
            self.palette.close();
            self.transcript_view.search.close();
        }
        self.viewport = next;
    }

    /// Leave Scroll mode and restore the prior viewport. Only switches the
    /// viewport: it does not return to the tail, so typing or opening a search
    /// from scroll mode keeps the view. Esc and End add follow_tail at their
    /// call sites for the tail-return behavior.
    pub fn exit_scroll(&mut self) {
        self.viewport = self.prev_viewport;
    }

    /// Fold Focus into Working (user pressed Esc in Focus). Stage unchanged.
    pub fn fold_to_working(&mut self) {
        self.viewport = ViewportMode::Working;
    }

    /// Pop the most recent stage off the history stack and restore it. Returns
    /// the restored stage when there was something to rewind to. Also syncs the
    /// viewport so rewinding from Focus back to design unfolds to Working.
    pub fn rewind_stage(&mut self) -> Option<Stage> {
        if self.workflow.stage_history.is_empty() {
            return None;
        }
        let prev = self.workflow.stage_history.pop().expect("non-empty");
        self.workflow.stage = prev;
        self.workflow.spec_ctx.step = prev.label().to_string();
        self.sync_viewport_to_stage();
        Some(prev)
    }
}
