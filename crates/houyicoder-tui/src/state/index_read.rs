//! The running index build: a background task reads one chunk of the session
//! log for the event-byte-offset index while the draw path stays off disk, and
//! ships its progress over a channel the record on file holds.
//!
//! A chunk reads up to four megabytes and parses every line in it, so it cannot
//! run where the frame is drawn. The loop advances the build instead: it drains
//! the record and dispatches the next chunk, which is the same shape the older
//! history read uses.

use crate::transcript::snapshot::IndexProgress;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::TryRecvError;

/// Index chunks reading the log at once.
///
/// A second dispatch drops the running chunk's receiver and orphans that
/// worker: it finishes its read and sends into a channel nobody holds. The
/// budget is asserted at the dispatch point rather than left to the shape of
/// the record, and measured in records because no path in the build drops one
/// while its worker still reads. Only replacing or dropping the whole app
/// does, which orphans one bounded chunk and leaves the next session counting
/// from zero.
pub(crate) const INDEX_CHUNK_BUDGET: usize = 1;

/// One index chunk while its task runs. The receiver is the channel the
/// background task sends progress over; the task owns the sender and drops it
/// on exit.
pub struct PendingIndexChunk {
    rx: Receiver<IndexProgress>,
}

impl std::fmt::Debug for PendingIndexChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingIndexChunk").finish_non_exhaustive()
    }
}

/// What a poll sees: still reading, progress ready to apply, or the task gone
/// (panicked or dropped its sender). Disconnected is distinct from Pending so a
/// dead worker cannot pin the record and stall the build forever.
#[derive(Debug)]
pub enum IndexChunkPoll {
    Pending,
    Ready(IndexProgress),
    Disconnected,
}

impl PendingIndexChunk {
    pub(crate) fn new(rx: Receiver<IndexProgress>) -> Self {
        Self { rx }
    }

    /// Drain the channel without blocking.
    pub fn poll(&self) -> IndexChunkPoll {
        match self.rx.try_recv() {
            Ok(progress) => IndexChunkPoll::Ready(progress),
            Err(TryRecvError::Empty) => IndexChunkPoll::Pending,
            Err(TryRecvError::Disconnected) => IndexChunkPoll::Disconnected,
        }
    }
}
