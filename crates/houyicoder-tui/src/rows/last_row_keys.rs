//! The per-row keys of the last emitted visible row set.
//!
//! A click or Ctrl+O maps a screen row straight to the key it acts on, so the
//! key vectors have to stay in step with the rows they were built for. All
//! four are taken from one row set in one operation, which is what keeps a
//! key from pointing at the row beside the one it was built for.

use crate::row_id::RowId;

use super::row_set::RowSet;

/// The call-id, fold-group key, expanded-group key, and turn id carried by
/// each visible row of the last render, indexed by visible row.
#[derive(Debug, Default)]
pub(crate) struct LastRowKeys {
    callids: Vec<Option<String>>,
    fold_keys: Vec<Option<String>>,
    expanded_group: Vec<Option<String>>,
    turn_ids: Vec<Option<RowId>>,
}

impl LastRowKeys {
    /// Take the keys of one row set, in row order. Every vector comes from the
    /// same set, so they cannot arrive at different lengths.
    pub(crate) fn replace(&mut self, set: &RowSet) {
        self.callids.clear();
        self.callids.extend_from_slice(set.callids());
        self.fold_keys.clear();
        self.fold_keys.extend_from_slice(set.fold_keys());
        self.expanded_group.clear();
        self.expanded_group.extend_from_slice(set.expanded_group());
        self.turn_ids.clear();
        self.turn_ids.extend_from_slice(set.turn_ids());
    }

    /// The marks of the rows inside an expanded group, in row order, for the
    /// render path's block background.
    pub(crate) fn expanded_group(&self) -> &[Option<String>] {
        &self.expanded_group
    }

    /// The fold-group keys of the visible rows, in row order, for a test that
    /// has to find the row a key landed on.
    #[cfg(test)]
    pub(crate) fn fold_keys(&self) -> &[Option<String>] {
        &self.fold_keys
    }

    /// The turn ids of the visible rows, in row order, for the same lookup.
    #[cfg(test)]
    pub(crate) fn turn_ids(&self) -> &[Option<RowId>] {
        &self.turn_ids
    }

    /// The call id a visible row carries, looked up by visible-row index.
    pub(crate) fn callid_at(&self, ri: usize) -> Option<String> {
        self.callids.get(ri).cloned().flatten()
    }

    /// The fold-group key a visible row belongs to, by visible-row index.
    pub(crate) fn fold_key_at(&self, ri: usize) -> Option<String> {
        self.fold_keys.get(ri).cloned().flatten()
    }

    /// The expanded-group mark a visible row carries, by visible-row index.
    pub(crate) fn expanded_group_at(&self, ri: usize) -> Option<String> {
        self.expanded_group.get(ri).cloned().flatten()
    }

    /// The ThoughtFor turn id a visible row carries, by visible-row index.
    pub(crate) fn turn_id_at(&self, ri: usize) -> Option<RowId> {
        self.turn_ids.get(ri).cloned().flatten()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.callids.is_empty()
    }

    /// Append a call id beyond the published rows. Deliberate fault
    /// injection: a test needs a key stash that outlived its rows to check
    /// how the resolver tolerates one.
    #[cfg(test)]
    pub(crate) fn push_callid_for_test(&mut self, callid: Option<String>) {
        self.callids.push(callid);
    }

    /// Append a fold key beyond the published rows, for the same drift.
    #[cfg(test)]
    pub(crate) fn push_fold_key_for_test(&mut self, key: Option<String>) {
        self.fold_keys.push(key);
    }
}
