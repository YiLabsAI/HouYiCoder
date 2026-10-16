//! The running epoch scan: a background task reads one reverse chunk of the
//! session log while the draw path stays off disk, and ships its progress over
//! a channel the record on file holds.
//!
//! A chunk reads up to four megabytes and parses every line in it, so it cannot
//! run where the frame is drawn. The loop advances the scan instead: it drains
//! the record and dispatches the next chunk, which is the same shape the older
//! history read uses.

use crate::transcript::snapshot::ScanProgress;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::fmt::Result;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::TryRecvError;

/// Scan chunks reading the log at once.
///
/// A second dispatch drops the running chunk's receiver and orphans that
/// worker: it finishes its read and sends into a channel nobody holds. The
/// budget is asserted at the dispatch point rather than left to the shape of
/// the record, and measured in records because no path in the scan drops one
/// while its worker still reads. Only replacing or dropping the whole app
/// does, which orphans one bounded chunk and leaves the next session counting
/// from zero.
pub(crate) const SCAN_CHUNK_BUDGET: usize = 1;

/// One scan chunk while its task runs. The receiver is the channel the
/// background task sends progress over; the task owns the sender and drops it
/// on exit.
pub struct PendingScanChunk {
    rx: Receiver<ScanProgress>,
}

impl Debug for PendingScanChunk {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        f.debug_struct("PendingScanChunk").finish_non_exhaustive()
    }
}

/// What a poll sees: still reading, progress ready to apply, or the task gone
/// (panicked or dropped its sender). Disconnected is distinct from Pending so a
/// dead worker cannot pin the record and stall the scan forever.
#[derive(Debug)]
pub enum ScanChunkPoll {
    Pending,
    Ready(ScanProgress),
    Disconnected,
}

impl PendingScanChunk {
    pub(crate) fn new(rx: Receiver<ScanProgress>) -> Self {
        Self { rx }
    }

    /// Drain the channel without blocking.
    pub fn poll(&self) -> ScanChunkPoll {
        match self.rx.try_recv() {
            Ok(progress) => ScanChunkPoll::Ready(progress),
            Err(TryRecvError::Empty) => ScanChunkPoll::Pending,
            Err(TryRecvError::Disconnected) => ScanChunkPoll::Disconnected,
        }
    }
}
