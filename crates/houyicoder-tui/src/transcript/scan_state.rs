//! The G full-scan's progress mirror and its pending chunk.

use std::cell::{Cell, RefCell};

use crate::state::scan_chunk::PendingScanChunk;

/// The G full-scan's state that the chrome reads and the loop advances. The
/// scan reads the log backward a chunk at a time, discovering where each
/// history began and giving the log's sidecar index its one check and rebuild;
/// it does not build the offsets itself — those answer from the sidecar file
/// whether or not the scan has run. A search-view exit resets the progress
/// cells; the pending chunk slot clears when its worker lands or dies.
#[derive(Default)]
pub(crate) struct ScanState {
    /// True while the G full-scan reads the log (one chunk at a time keeps the
    /// UI responsive; Esc interrupts). The loop advances the scan while this
    /// is set, off the draw path.
    pub(crate) indexing: Cell<bool>,
    /// Bytes of the log the reverse scan has covered so far, for the
    /// scanning-percent chrome, published as each chunk lands.
    pub(crate) scanned_bytes: Cell<u64>,
    /// Total log bytes the scan covers, the frozen file size.
    pub(crate) scan_total: Cell<u64>,
    /// The scan chunk dispatched and not yet applied, if any. A chunk reads
    /// the log, so the loop advances the scan and the draw path never does.
    pub(crate) pending_scan_chunk: RefCell<Option<PendingScanChunk>>,
}
