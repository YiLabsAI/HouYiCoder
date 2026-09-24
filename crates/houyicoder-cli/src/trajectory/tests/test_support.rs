//! Backends the trajectory read tests drive.
//!
//! A read test needs the log to be real and the timing to be the test's: the
//! window must be able to move, or an append must be able to land, while one
//! read is still in flight. Each backend here is a real one with a single
//! method replaced, so everything except the held call behaves as it does in a
//! session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};

use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, EventId, LogRangeRead,
    ReverseRead, SessionId, SessionLogEntry,
};
use houyicoder_memory::LocalFileBackend;

/// The future type the backend trait spells, without depending on the crate
/// that names its alias.
type PFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// A file backend whose first range read waits for the test, so an append or a
/// window move can land while a read is in flight.
///
/// Only the first read waits: the reads that follow are the ones the test wants
/// to run meanwhile, and holding them too would deadlock the test against
/// itself.
pub(super) struct GatedRangeBackend {
    inner: LocalFileBackend,
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    held: AtomicBool,
}

/// A gated backend and the two ends of its handshake: wait on the receiver to
/// know a range read has started, send on the sender (or drop it) to let the
/// read finish.
pub(super) fn gated_range_backend(
    root: std::path::PathBuf,
) -> (GatedRangeBackend, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let backend = GatedRangeBackend {
        inner: LocalFileBackend::new(root),
        entered: entered_tx,
        release: Mutex::new(release_rx),
        held: AtomicBool::new(false),
    };
    (backend, entered_rx, release_tx)
}

impl ContextBackend for GatedRangeBackend {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        self.inner.append(event)
    }
    fn read_range(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.inner.read_range(session, from, to)
    }
    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.inner.replay(session)
    }
    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        self.inner.write_checkpoint(manifest)
    }
    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        self.inner.read_checkpoint(id)
    }
    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        self.inner.list_checkpoints(session)
    }
    fn supports_log_windows(&self) -> bool {
        true
    }
    fn log_size(&self, session: SessionId) -> u64 {
        self.inner.log_size(session)
    }
    fn read_lines_reverse(&self, session: SessionId, from: u64, max: u64) -> ReverseRead {
        self.inner.read_lines_reverse(session, from, max)
    }
    fn read_log_range(&self, session: SessionId, from: u64, max: u64) -> LogRangeRead {
        if !self.held.swap(true, Ordering::SeqCst) {
            self.entered.send(()).ok();
            self.release.lock().expect("release lock").recv().ok();
        }
        self.inner.read_log_range(session, from, max)
    }
}
