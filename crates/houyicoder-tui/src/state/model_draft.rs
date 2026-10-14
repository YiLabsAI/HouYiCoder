//! The /model pane's draft: what Enter would commit. The draft is the only
//! thing keys mutate, so a discarded or failed pick cannot leave the pane
//! claiming a switch that never happened. The catalog facts the draft
//! resolves against — rows, capabilities, effort chains — are the session's
//! model capability; the compositions across the two owners are App methods,
//! and the mutation methods here take the facts they need as arguments.

use houyicoder_protocol::frontend::model::SpeedMode;
use houyicoder_protocol::llm::EffortLevel;

/// Which setting the pane's Tab focus is on. Only one setting takes the
/// arrows at a time; Up/Down never leave the model list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelSettingFocus {
    Effort,
    Fast,
}

/// What Enter would commit. Nothing here is applied until the host replies:
/// the pane renders the draft, the transcript renders the reply.
#[derive(Debug, Clone)]
pub(crate) struct ModelDraft {
    /// The model list row the focus is on: 0 is the Default sentinel, row i
    /// is catalog entry i-1.
    pub(crate) row: usize,
    /// The level this draft would send. None = no effort parameter.
    pub(crate) effort: Option<EffortLevel>,
    /// Whether the user adjusted the level, which drops the default marker.
    pub(crate) effort_touched: bool,
    /// The speed tier this draft would send.
    pub(crate) speed: SpeedMode,
    /// The setting the arrows adjust.
    pub(crate) focus: ModelSettingFocus,
    /// Whether the user changed anything. A fresh catalog landing while the
    /// pane is open re-seeds a clean draft but leaves a dirty one alone.
    pub(crate) dirty: bool,
}

impl Default for ModelDraft {
    fn default() -> Self {
        Self {
            row: 0,
            effort: None,
            effort_touched: false,
            speed: SpeedMode::Standard,
            focus: ModelSettingFocus::Effort,
            dirty: false,
        }
    }
}

impl ModelDraft {
    /// Reset the draft to a clean copy of what the session applies: the row
    /// the session is set to, the level that row's chain resolves, the
    /// applied tier, focus on effort.
    pub(crate) fn reseed(&mut self, row: usize, speed: SpeedMode, effort: Option<EffortLevel>) {
        self.row = row;
        self.speed = speed;
        self.focus = ModelSettingFocus::Effort;
        self.effort_touched = false;
        self.dirty = false;
        self.effort = effort;
    }

    /// Move the model focus by a signed delta against the row count, clamped
    /// with no wrap. Returns false when the row did not change; the caller
    /// re-resolves the level from the new row's chain only when it did, so an
    /// explicit pick made on one row does not carry to another.
    pub(crate) fn move_focus(&mut self, delta: isize, rows: usize) -> bool {
        let last = rows.saturating_sub(1);
        let next = (self.row as isize + delta).clamp(0, last as isize) as usize;
        if next == self.row {
            // A keypress that cannot move (Up at the top) is not a draft
            // change: wiping the effort adjustment would discard work.
            return false;
        }
        self.row = next;
        self.effort_touched = false;
        self.dirty = true;
        true
    }

    /// Move the setting focus to the other adjustable setting, skipping one
    /// the focused model does not support. A model with neither keeps the
    /// focus on effort, which the pane renders as unavailable.
    pub(crate) fn cycle_focus(&mut self, effort_available: bool, fast_available: bool) {
        self.focus = match (effort_available, fast_available) {
            (false, true) => ModelSettingFocus::Fast,
            (true, true) => match self.focus {
                ModelSettingFocus::Effort => ModelSettingFocus::Fast,
                ModelSettingFocus::Fast => ModelSettingFocus::Effort,
            },
            _ => ModelSettingFocus::Effort,
        };
    }

    /// Cycle the level through the levels the model supports, in order: auto
    /// (no effort parameter) is a real stop rather than a state the user can
    /// never reach again after the first adjustment. A no-op when the focused
    /// model speaks no effort dialect.
    pub(crate) fn adjust_effort(&mut self, levels: &[EffortLevel], forward: bool) {
        if levels.is_empty() {
            return;
        }
        let len = levels.len() + 1;
        let idx = match self.effort {
            None => 0,
            Some(current) => levels
                .iter()
                .position(|level| *level == current)
                .map_or(0, |i| i + 1),
        };
        let next = if forward {
            (idx + 1) % len
        } else {
            (idx + len - 1) % len
        };
        self.effort = if next == 0 {
            None
        } else {
            Some(levels[next - 1])
        };
        self.effort_touched = true;
        self.dirty = true;
    }

    /// Toggle the tier off/on. A no-op when the focused model does not offer
    /// the tier at all.
    pub(crate) fn toggle_speed(&mut self, fast_available: bool) {
        if !fast_available {
            return;
        }
        self.speed = match self.speed {
            SpeedMode::Standard => SpeedMode::Fast,
            SpeedMode::Fast => SpeedMode::Standard,
        };
        self.dirty = true;
    }

    /// The tier this draft would apply: a tier the focused model cannot
    /// serve is not one of them, so the draft reports off rather than asking
    /// for a tier the host would drop.
    pub(crate) fn effective_speed(&self, fast_available: bool) -> SpeedMode {
        if self.fast_forced_off(fast_available) {
            SpeedMode::Standard
        } else {
            self.speed
        }
    }

    /// Whether the draft keeps Fast on only because the focused model cannot
    /// serve it. The pane says so rather than showing a tier that will not be
    /// applied.
    pub(crate) fn fast_forced_off(&self, fast_available: bool) -> bool {
        self.speed == SpeedMode::Fast && !fast_available
    }
}

#[cfg(test)]
mod tests;
