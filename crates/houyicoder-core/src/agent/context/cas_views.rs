//! The forms each assembly used for its stored blocks.
//!
//! A block the assembler reaches twice within one assembly keeps one record,
//! so a continuation that reuses its assembly id reports the same views
//! instead of counting a block twice.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use houyicoder_context::{CachedModelView, ViewForm};

/// The forms every recorded assembly carried, keyed by assembly and then by block.
///
/// The estimate unit is fixed when the host builds the record, so the figures
/// it collects are labelled by the policy that produced them rather than
/// assumed to count tokens.
#[derive(Debug)]
pub(crate) struct CasViewState {
    estimate_unit: &'static str,
    views: Mutex<BTreeMap<u64, BTreeMap<String, CachedModelView>>>,
}

impl CasViewState {
    /// Build a record whose figures count in the unit the policy reports.
    pub(crate) fn new(estimate_unit: &'static str) -> Self {
        Self {
            estimate_unit,
            views: Mutex::new(BTreeMap::new()),
        }
    }

    /// The unit the recorded figures count in.
    pub(crate) fn estimate_unit(&self) -> &'static str {
        self.estimate_unit
    }

    /// Record the form one assembly used for one block. A later record for
    /// the same block in the same assembly replaces the earlier one.
    pub(crate) fn record(&self, assembly_id: u64, block_ref: &str, form: ViewForm, size: u64) {
        let mut views = self.views.lock().unwrap_or_else(PoisonError::into_inner);
        views.entry(assembly_id).or_default().insert(
            block_ref.to_string(),
            CachedModelView {
                block_ref: block_ref.to_string(),
                form,
                size,
            },
        );
    }

    /// Take every recorded assembly's views, lowest assembly first, so the
    /// runner writes one event per assembly. The record is left empty.
    pub(crate) fn take(&self) -> Vec<(u64, Vec<CachedModelView>)> {
        let mut views = self.views.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut *views)
            .into_iter()
            .map(|(assembly_id, per_block)| (assembly_id, per_block.into_values().collect()))
            .collect()
    }
}

#[cfg(test)]
mod tests;
