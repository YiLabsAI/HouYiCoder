//! The /model picker: the host snapshot, the user's draft, and the commit in
//! flight. The draft is the only thing keys mutate, so a discarded or failed
//! pick cannot leave the pane claiming a switch that never happened.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::model::{
    EffortCapability, ModelCatalog, ModelCatalogEntry, ModelChoice, ModelDisplayCapabilities,
    SpeedMode,
};
use houyicoder_protocol::llm::EffortLevel;

/// The display name of the Default sentinel. Picking it resolves the default
/// model, so the row is named for the choice rather than for the resolved id.
pub const DEFAULT_LABEL: &str = "Default (recommended)";

/// The effort levels available on the focused model, from the host's
/// capability report. Empty when the model speaks no effort dialect.
fn effort_levels_of(capability: &EffortCapability) -> Vec<EffortLevel> {
    match capability {
        EffortCapability::Supported { levels } => levels.clone(),
        EffortCapability::Unsupported => Vec::new(),
    }
}

/// Which setting the pane's Tab focus is on. Only one setting takes the
/// arrows at a time; Up/Down never leave the model list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSettingFocus {
    Effort,
    Fast,
}

/// What Enter would commit. Nothing here is applied until the host replies:
/// the pane renders the draft, the transcript renders the reply.
#[derive(Debug, Clone)]
pub struct ModelDraft {
    /// The model list row the focus is on: 0 is the Default sentinel, row i
    /// is catalog entry i-1.
    pub row: usize,
    /// The level this draft would send. None = no effort parameter.
    pub effort: Option<EffortLevel>,
    /// Whether the user adjusted the level, which drops the default marker.
    pub effort_touched: bool,
    /// The speed tier this draft would send.
    pub speed: SpeedMode,
    /// The setting the arrows adjust.
    pub focus: ModelSettingFocus,
    /// Whether the user changed anything. A snapshot landing while the pane
    /// is open re-seeds a clean draft but leaves a dirty one alone.
    pub dirty: bool,
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

/// A commit awaiting its reply. The pane stays open and Enter is ignored
/// until the reply or an error settles it, so a second press cannot ship a
/// duplicate switch.
#[derive(Debug, Clone)]
pub struct PendingCommit {
    pub req_id: RequestId,
    /// The tier the session ran at before this commit, so the receipt reports
    /// Fast only when the pick moved it or the target model can serve it.
    pub prior_speed: SpeedMode,
}

/// The /model picker state.
#[derive(Debug, Clone, Default)]
pub struct ModelPickerState {
    /// The host's answer to the last query: catalog rows, live selection,
    /// applied model, Default target, global effort fallback.
    pub snapshot: ModelCatalog,
    pub draft: ModelDraft,
    /// The commit with the host, held until its reply lands. It carries the
    /// request id the reply must match and the tier to fall back to when the
    /// reply brings no snapshot.
    pub pending_request: Option<PendingCommit>,
}

impl ModelPickerState {
    /// The rows the pane lists: the Default sentinel plus the catalog.
    pub fn rows(&self) -> usize {
        self.snapshot.entries.len() + 1
    }

    /// The catalog entry for a row, or None for the Default sentinel and for
    /// any row past the list.
    pub fn entry_at(&self, row: usize) -> Option<&ModelCatalogEntry> {
        row.checked_sub(1)
            .and_then(|i| self.snapshot.entries.get(i))
    }

    /// The entry for an id, whatever row it sits on.
    pub fn entry_for_id(&self, id: &str) -> Option<&ModelCatalogEntry> {
        self.snapshot.entries.iter().find(|e| e.id == id)
    }

    /// The row an id occupies, or None when the catalog does not list it.
    pub fn row_for_id(&self, id: &str) -> Option<usize> {
        self.snapshot
            .entries
            .iter()
            .position(|e| e.id == id)
            .map(|i| i + 1)
    }

    /// The focused entry, or None on the Default row.
    pub fn focused(&self) -> Option<&ModelCatalogEntry> {
        self.entry_at(self.draft.row)
    }

    /// The host-resolved capabilities of the focused row.
    pub fn focused_capabilities(&self) -> ModelDisplayCapabilities {
        match self.focused() {
            Some(entry) => entry.capabilities.clone(),
            None if self.draft.row == 0 => self.snapshot.resolved_default.capabilities.clone(),
            None => ModelDisplayCapabilities::default(),
        }
    }

    /// The host-resolved capabilities of an id, taken from the row carrying
    /// it.
    pub fn capabilities_for(&self, id: &str) -> ModelDisplayCapabilities {
        if id == self.snapshot.resolved_default.id {
            return self.snapshot.resolved_default.capabilities.clone();
        }
        match self.entry_for_id(id) {
            Some(entry) => entry.capabilities.clone(),
            None => ModelDisplayCapabilities::default(),
        }
    }

    /// The label a receipt names for an applied choice: the Default sentinel
    /// reads as the choice, not as the settings id it replaced, and a catalog
    /// row reads as the name the user picked rather than its id.
    pub fn label_for(&self, choice: &ModelChoice) -> String {
        match choice {
            ModelChoice::Default => "Default".to_string(),
            ModelChoice::Explicit { id } => match self.entry_for_id(id) {
                Some(entry) => entry.label().to_string(),
                None => id.clone(),
            },
        }
    }

    /// The selection the focused row would commit.
    pub fn focused_choice(&self) -> ModelChoice {
        match self.focused() {
            Some(entry) => ModelChoice::Explicit {
                id: entry.id.clone(),
            },
            None => ModelChoice::Default,
        }
    }

    /// The level the chain resolves for a row: the row's saved per-model pick,
    /// then the global fallback, then nothing (the host sends no effort
    /// parameter). The Default row resolves through the entry carrying the
    /// default model's id.
    pub fn chain_effort(&self, row: usize) -> Option<EffortLevel> {
        // The pane shows only levels the model accepts: a persisted level
        // above the model's set clamps to the set's top, the same clamp the
        // host applies before sending.
        let clamp = |level: EffortLevel, capability: &EffortCapability| -> Option<EffortLevel> {
            let levels = effort_levels_of(capability);
            levels
                .iter()
                .copied()
                .rev()
                .find(|l| *l <= level)
                .or_else(|| levels.first().copied())
        };
        match row {
            // Row 0 is the Default sentinel: its chain seed reads the
            // resolved default's entry, but its clamp set comes from the
            // resolved default's capabilities — the default model often has
            // no catalog row of its own, and an empty set would hide a level
            // the host would send.
            0 => {
                let default = &self.snapshot.resolved_default;
                let level = self
                    .entry_for_id(&default.id)
                    .and_then(|e| e.effort)
                    .or(self.snapshot.effort_level)?;
                clamp(level, &default.capabilities.effort)
            }
            _ => {
                let entry = self.entry_at(row)?;
                let level = entry.effort.or(self.snapshot.effort_level)?;
                clamp(level, &entry.capabilities.effort)
            }
        }
    }

    /// The speed the draft would apply: a tier the focused model cannot serve
    /// is not one of them, so the draft reports off rather than asking for a
    /// tier the host would drop.
    pub fn effective_speed(&self) -> SpeedMode {
        if self.fast_forced_off() {
            SpeedMode::Standard
        } else {
            self.draft.speed
        }
    }

    /// Whether the draft keeps Fast on only because the focused model cannot
    /// serve it. The pane says so rather than showing a tier that will not be
    /// applied.
    pub fn fast_forced_off(&self) -> bool {
        self.draft.speed == SpeedMode::Fast && !self.focused_capabilities().fast.is_available()
    }

    /// Whether a commit is awaiting its reply.
    pub fn is_pending(&self) -> bool {
        self.pending_request.is_some()
    }

    /// Reset the draft from the snapshot: the row the session is set to, the
    /// level that row's chain resolves, the applied tier, focus on effort.
    pub fn reseed(&mut self) {
        self.draft.row = self.row_for_choice(&self.snapshot.selected);
        self.draft.speed = self.snapshot.applied.speed;
        self.draft.focus = ModelSettingFocus::Effort;
        self.draft.effort_touched = false;
        self.draft.dirty = false;
        self.reseed_effort();
    }

    /// Take a fresh snapshot, keeping a dirty draft: the user's in-progress
    /// pick wins over a query reply that raced it.
    pub fn refresh_snapshot(&mut self, catalog: ModelCatalog) {
        self.snapshot = catalog;
        if !self.draft.dirty {
            self.reseed();
        }
    }

    /// Settle the commit: a reply carrying a snapshot replaces it and the
    /// draft re-seeds, so the pane shows what the host applied. An error
    /// settles without a snapshot and keeps the draft for a retry.
    pub fn settle(&mut self, catalog: Option<ModelCatalog>) {
        self.pending_request = None;
        if let Some(catalog) = catalog {
            self.draft.dirty = false;
            self.refresh_snapshot(catalog);
        }
    }

    /// Move the model focus. The level follows the newly focused model's
    /// chain: an explicit pick made on one row does not carry to another.
    pub fn move_focus(&mut self, delta: isize) {
        let last = self.rows().saturating_sub(1);
        let next = (self.draft.row as isize + delta).clamp(0, last as isize) as usize;
        if next == self.draft.row {
            // A keypress that cannot move (Up at the top) is not a draft
            // change: wiping the effort adjustment would discard work.
            return;
        }
        self.draft.row = next;
        self.draft.effort_touched = false;
        self.draft.dirty = true;
        self.reseed_effort();
    }

    /// Move the setting focus to the other adjustable setting, skipping one
    /// the focused model does not support. A model with neither keeps the
    /// focus on effort, which the pane renders as unavailable.
    pub fn cycle_setting_focus(&mut self) {
        let effort_ok = self.effort_supported();
        let fast_ok = self.focused_capabilities().fast.is_available();
        self.draft.focus = match (effort_ok, fast_ok) {
            (false, true) => ModelSettingFocus::Fast,
            (true, true) => match self.draft.focus {
                ModelSettingFocus::Effort => ModelSettingFocus::Fast,
                ModelSettingFocus::Fast => ModelSettingFocus::Effort,
            },
            _ => ModelSettingFocus::Effort,
        };
    }

    /// Adjust the focused setting: the level wraps through low/medium/high,
    /// the tier toggles off/on. Both are no-ops when the focused model does
    /// not support the setting.
    pub fn adjust_setting(&mut self, forward: bool) {
        match self.draft.focus {
            ModelSettingFocus::Effort => {
                let levels = effort_levels_of(&self.focused_capabilities().effort);
                if levels.is_empty() {
                    return;
                }
                // The current level, or the middle of the served set for a
                // draft that follows the chain: one press lands either side.
                let idx = self
                    .draft
                    .effort
                    .and_then(|current| levels.iter().position(|level| *level == current))
                    .unwrap_or(levels.len() / 2);
                let next = if forward {
                    (idx + 1) % levels.len()
                } else {
                    (idx + levels.len() - 1) % levels.len()
                };
                self.draft.effort = Some(levels[next]);
                self.draft.effort_touched = true;
                self.draft.dirty = true;
            }
            ModelSettingFocus::Fast => {
                if !self.focused_capabilities().fast.is_available() {
                    return;
                }
                self.draft.speed = match self.draft.speed {
                    SpeedMode::Standard => SpeedMode::Fast,
                    SpeedMode::Fast => SpeedMode::Standard,
                };
                self.draft.dirty = true;
            }
        }
    }

    /// Whether the focused model has any effort level available.
    pub fn effort_supported(&self) -> bool {
        !effort_levels_of(&self.focused_capabilities().effort).is_empty()
    }

    /// The row a selection occupies, falling back to the Default row when the
    /// catalog does not list it.
    fn row_for_choice(&self, choice: &ModelChoice) -> usize {
        match choice {
            ModelChoice::Default => 0,
            ModelChoice::Explicit { id } => self.row_for_id(id).unwrap_or(0),
        }
    }

    /// Re-resolve the draft level for the focused row, unless the user set it.
    /// A model that speaks no effort dialect carries no level at all.
    fn reseed_effort(&mut self) {
        self.draft.effort = if self.effort_supported() {
            self.chain_effort(self.draft.row)
        } else {
            None
        };
    }
}
