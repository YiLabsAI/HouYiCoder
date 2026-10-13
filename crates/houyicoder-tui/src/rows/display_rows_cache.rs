//! The slot cache: the row set the draw pass pre-renders and keeps until its
//! identity goes stale.
//!
//! One owner because the rows and the key that invalidates them are read and
//! written together. The cache holds the whole set behind one borrow, so a
//! reader that walks the visible window sees one build's rows and metadata.

use std::cell::RefCell;

use super::row_set::RowSet;
use super::slots_key::SlotsKey;

/// The cached slot rows and the key they were built for. The set sits behind
/// one borrow so the draw pass reads the rows and the metadata of one build
/// in a single pass.
#[derive(Debug, Default)]
pub(crate) struct DisplayRowsCache {
    slots: RefCell<RowSet>,
    /// The key the cached rows were built for, or None before the first
    /// build. The draw pass compares it to the key it would build and
    /// rebuilds on mismatch; a session switch clears it so the new session
    /// always rebuilds first.
    key: RefCell<Option<SlotsKey>>,
}

impl DisplayRowsCache {
    /// Whether the cached rows were built for this key. False before the
    /// first build and after a reset, so both rebuild.
    pub(crate) fn holds(&self, key: &SlotsKey) -> bool {
        self.key.borrow().as_ref() == Some(key)
    }

    /// Read the cached rows. The borrow covers the whole set, so a reader
    /// cannot combine rows from one build with metadata from another.
    pub(crate) fn with<R>(&self, read: impl FnOnce(&RowSet) -> R) -> R {
        read(&self.slots.borrow())
    }

    /// Replace the cached rows and the key they were built for in one
    /// operation. A reader never observes the new rows under the old key or
    /// the reverse.
    pub(crate) fn replace(&self, key: SlotsKey, slots: RowSet) {
        *self.slots.borrow_mut() = slots;
        *self.key.borrow_mut() = Some(key);
    }

    /// Drop the cached rows and the key, so the next draw rebuilds rather
    /// than reusing the previous session's rows.
    pub(crate) fn reset(&self) {
        *self.slots.borrow_mut() = RowSet::default();
        *self.key.borrow_mut() = None;
    }
}
