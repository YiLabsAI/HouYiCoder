//! The per-row keys of the last emitted visible row set, kept parallel to
//! the rows so a click or toggle maps a screen row straight to the key it
//! acts on without re-counting position. One owner because the four vectors
//! are written together by the draw pass and read together by the click and
//! Ctrl+O handlers: keeping them flat invited a write that moved one vector
//! and left the others at the old length, desyncing the index.

use std::cell::RefCell;

use crate::row_id::RowId;

/// The call-id, fold-group key, expanded-group key, and turn id carried by
/// each visible row of the last render. Every vector is indexed by visible
/// row and written in lockstep by the draw pass.
#[derive(Debug, Default)]
pub(crate) struct LastRowKeys {
    /// The tool-call id a visible row carries (Some on a result row) so Ctrl+O
    /// maps the anchor's row to the result to toggle, without changing the
    /// (u8, String) copy tuple the row cache holds.
    pub(crate) callids: RefCell<Vec<Option<String>>>,
    /// The fold-group key a visible row belongs to (Some on a collapsed
    /// summary or expanded collapse-hint row) so Ctrl+O and click can toggle
    /// the fold group under the selection anchor.
    pub(crate) fold_keys: RefCell<Vec<Option<String>>>,
    /// Marks only rows inside an expanded fold group (the expanded summary
    /// header plus every body row the group emits). Consumed by the
    /// click-release handler (click anywhere in an expanded block collapses
    /// it) and the render path (gray background on the expanded block). Kept
    /// separate from fold_keys so Ctrl+O's result-first contract stays intact:
    /// Ctrl+O reads fold_keys (None on body rows) and expands the result under
    /// the cursor, never collapses the group from a body row.
    pub(crate) expanded_group: RefCell<Vec<Option<String>>>,
    /// The ThoughtFor turn id a visible row carries (Some on a "Thought for
    /// Ns" header row, None elsewhere) so the click handler maps a click's row
    /// straight to the turn id to toggle, without counting
    /// nth-visible-then-nth-in-full-transcript (which misaligned when
    /// ThoughtFor rows scrolled out of the viewport: the visible count skipped
    /// off-screen rows while the full-transcript count did not, so clicking a
    /// visible row toggled an off-screen turn).
    pub(crate) turn_ids: RefCell<Vec<Option<RowId>>>,
}

impl LastRowKeys {
    /// The call id a visible row carries, looked up by visible-row index.
    pub(crate) fn callid_at(&self, ri: usize) -> Option<String> {
        self.callids.borrow().get(ri).cloned().flatten()
    }

    /// The fold-group key a visible row belongs to, by visible-row index.
    pub(crate) fn fold_key_at(&self, ri: usize) -> Option<String> {
        self.fold_keys.borrow().get(ri).cloned().flatten()
    }

    /// The expanded-group mark a visible row carries, by visible-row index.
    pub(crate) fn expanded_group_at(&self, ri: usize) -> Option<String> {
        self.expanded_group.borrow().get(ri).cloned().flatten()
    }

    /// The ThoughtFor turn id a visible row carries, by visible-row index.
    pub(crate) fn turn_id_at(&self, ri: usize) -> Option<RowId> {
        self.turn_ids.borrow().get(ri).cloned().flatten()
    }
}
