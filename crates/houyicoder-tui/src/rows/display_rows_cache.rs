//! The slot cache: the row set the draw pass pre-renders and keeps until its
//! version goes stale.
//!
//! One owner because the rows and the version that invalidates them are read
//! and written together. The cache holds the whole set behind one borrow, so
//! a reader that walks the visible window sees one build's rows and metadata.

use std::cell::{Cell, RefCell};

use super::row_set::RowSet;

/// The cached slot rows and the version they were built at. The set sits
/// behind one borrow so the draw pass reads the rows and the metadata of one
/// build in a single pass.
#[derive(Debug, Default)]
pub(crate) struct DisplayRowsCache {
    slots: RefCell<RowSet>,
    /// The version the cached rows were built at. The draw pass compares it
    /// to the version it would build and rebuilds on mismatch; a session
    /// switch stamps the sentinel so the new session always rebuilds first.
    version: Cell<u64>,
}

impl DisplayRowsCache {
    /// The version the cached rows were built at.
    pub(crate) fn version(&self) -> u64 {
        self.version.get()
    }

    /// Read the cached rows. The borrow covers the whole set, so a reader
    /// cannot combine rows from one build with metadata from another.
    pub(crate) fn with<R>(&self, read: impl FnOnce(&RowSet) -> R) -> R {
        read(&self.slots.borrow())
    }

    /// Replace the cached rows and the version they were built at in one
    /// operation. A reader never observes the new rows under the old version
    /// or the reverse.
    pub(crate) fn replace(&self, version: u64, slots: RowSet) {
        *self.slots.borrow_mut() = slots;
        self.version.set(version);
    }

    /// Drop the cached rows and stamp the sentinel so the next draw rebuilds
    /// rather than reusing the previous session's rows.
    pub(crate) fn reset(&self) {
        *self.slots.borrow_mut() = RowSet::default();
        self.version.set(u64::MAX);
    }
}
