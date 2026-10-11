//! The session's model facts: the catalog the host reports, the applied
//! model, and the commit awaiting its reply. The pane's draft — the row the
//! focus sits on and the settings the user adjusted — is interaction state
//! and lives on the Ui owner; the compositions across the two are App
//! methods. Everything here is a fact about what the host said: the query
//! methods resolve rows, capabilities, labels, and effort chains against the
//! catalog without touching the draft.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::model::{
    EffortCapability, ModelCatalog, ModelCatalogEntry, ModelChoice, ModelDisplayCapabilities,
    SpeedMode,
};
use houyicoder_protocol::llm::EffortLevel;

/// A commit awaiting its reply. The pane stays open and Enter is ignored
/// until the reply or an error settles it, so a second press cannot ship a
/// duplicate switch.
#[derive(Debug, Clone)]
pub(crate) struct PendingCommit {
    pub(crate) req_id: RequestId,
    /// The tier the session ran at before this commit, so the receipt reports
    /// Fast only when the pick moved it or the target model offers it.
    pub(crate) prior_speed: SpeedMode,
}

/// The session's model capability: the host's answer to the last query
/// (catalog rows, live selection, applied model, Default target, global
/// effort fallback) and the commit in flight. A switch resets it by
/// construction, so a reply for the old session's commit can never settle
/// against the new one.
#[derive(Debug, Default)]
pub(crate) struct ModelCapability {
    /// The catalog the host last reported.
    pub(crate) catalog: ModelCatalog,
    /// The commit with the host, held until its reply lands.
    pub(crate) pending: Option<PendingCommit>,
}

/// The effort levels available on a model, from the host's capability report.
/// Empty when the model speaks no effort dialect.
fn effort_levels_of(capability: &EffortCapability) -> Vec<EffortLevel> {
    match capability {
        EffortCapability::Supported { levels } => levels.clone(),
        EffortCapability::Unsupported => Vec::new(),
    }
}

impl ModelCapability {
    /// The rows the pane lists: the Default sentinel plus the catalog.
    pub(crate) fn rows(&self) -> usize {
        self.catalog.entries.len() + 1
    }

    /// The catalog entry for a row, or None for the Default sentinel and for
    /// any row past the list.
    pub(crate) fn entry_at(&self, row: usize) -> Option<&ModelCatalogEntry> {
        row.checked_sub(1).and_then(|i| self.catalog.entries.get(i))
    }

    /// The entry for an id, whatever row it sits on.
    pub(crate) fn entry_for_id(&self, id: &str) -> Option<&ModelCatalogEntry> {
        self.catalog.entries.iter().find(|e| e.id == id)
    }

    /// The row an id occupies, or None when the catalog does not list it.
    pub(crate) fn row_for_id(&self, id: &str) -> Option<usize> {
        self.catalog
            .entries
            .iter()
            .position(|e| e.id == id)
            .map(|i| i + 1)
    }

    /// The host-resolved capabilities of a row.
    pub(crate) fn capabilities_at(&self, row: usize) -> ModelDisplayCapabilities {
        match self.entry_at(row) {
            Some(entry) => entry.capabilities.clone(),
            None if row == 0 => self.catalog.resolved_default.capabilities.clone(),
            None => ModelDisplayCapabilities::default(),
        }
    }

    /// The host-resolved capabilities of an id, taken from the row carrying
    /// it.
    pub(crate) fn capabilities_for(&self, id: &str) -> ModelDisplayCapabilities {
        if id == self.catalog.resolved_default.id {
            return self.catalog.resolved_default.capabilities.clone();
        }
        match self.entry_for_id(id) {
            Some(entry) => entry.capabilities.clone(),
            None => ModelDisplayCapabilities::default(),
        }
    }

    /// The label a receipt names for an applied choice: the Default sentinel
    /// reads as the choice, not as the settings id it replaced, and a catalog
    /// row reads as the name the user picked rather than its id.
    pub(crate) fn label_for(&self, choice: &ModelChoice) -> String {
        match choice {
            ModelChoice::Default => "Default".to_string(),
            ModelChoice::Explicit { id } => match self.entry_for_id(id) {
                Some(entry) => entry.label().to_string(),
                None => id.clone(),
            },
        }
    }

    /// The selection a row would commit.
    pub(crate) fn choice_at(&self, row: usize) -> ModelChoice {
        match self.entry_at(row) {
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
    pub(crate) fn chain_effort(&self, row: usize) -> Option<EffortLevel> {
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
                let default = &self.catalog.resolved_default;
                let level = self
                    .entry_for_id(&default.id)
                    .and_then(|e| e.effort)
                    .or(self.catalog.effort_level)?;
                clamp(level, &default.capabilities.effort)
            }
            _ => {
                let entry = self.entry_at(row)?;
                let level = entry.effort.or(self.catalog.effort_level)?;
                clamp(level, &entry.capabilities.effort)
            }
        }
    }

    /// Whether a row's model has any effort level available.
    pub(crate) fn effort_supported_at(&self, row: usize) -> bool {
        !effort_levels_of(&self.capabilities_at(row).effort).is_empty()
    }

    /// The effort levels a row's model offers, in the order the pane cycles
    /// them.
    pub(crate) fn effort_levels_at(&self, row: usize) -> Vec<EffortLevel> {
        effort_levels_of(&self.capabilities_at(row).effort)
    }

    /// Whether a row's model can use the Fast tier.
    pub(crate) fn fast_available_at(&self, row: usize) -> bool {
        self.capabilities_at(row).fast.is_available()
    }

    /// The level a freshly seeded draft holds for a row: the chain's
    /// resolution when the model speaks an effort dialect, none when it does
    /// not.
    pub(crate) fn seed_effort(&self, row: usize) -> Option<EffortLevel> {
        if self.effort_supported_at(row) {
            self.chain_effort(row)
        } else {
            None
        }
    }

    /// The row a selection occupies, falling back to the Default row when the
    /// catalog does not list it.
    pub(crate) fn row_for_choice(&self, choice: &ModelChoice) -> usize {
        match choice {
            ModelChoice::Default => 0,
            ModelChoice::Explicit { id } => self.row_for_id(id).unwrap_or(0),
        }
    }

    /// Whether a commit is awaiting its reply.
    pub(crate) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}
