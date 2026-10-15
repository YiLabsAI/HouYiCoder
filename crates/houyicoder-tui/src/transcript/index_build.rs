//! The G full-scan index build's progress mirror and its pending chunk.

use std::cell::{Cell, RefCell};

use crate::state::index_read::PendingIndexChunk;

/// The G full-scan build's state that the chrome reads and the loop advances.
/// A search-view exit resets the four progress cells; the pending chunk slot
/// clears when its worker lands or dies.
#[derive(Default)]
pub(crate) struct IndexBuild {
    /// True while the G full-scan builds the event-byte-offset index (one
    /// chunk at a time keeps the UI responsive; Esc interrupts). The loop
    /// advances the build while this is set, off the draw path.
    pub(crate) indexing: Cell<bool>,
    /// Bytes of the log indexed so far, for the indexing-percent chrome,
    /// published as each chunk lands.
    pub(crate) indexed_bytes: Cell<u64>,
    /// Total log bytes the index covers, the frozen file size.
    pub(crate) index_total: Cell<u64>,
    /// True when the full index is built, so the byte-offset lookup answers.
    pub(crate) index_done: Cell<bool>,
    /// The index chunk dispatched and not yet applied, if any. A chunk reads
    /// the log, so the loop advances the build and the draw path never does.
    pub(crate) pending_index_chunk: RefCell<Option<PendingIndexChunk>>,
}
