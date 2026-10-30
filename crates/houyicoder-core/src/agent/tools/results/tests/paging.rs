//! Tests for the page shapes a log may answer with while the index folds.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use houyicoder_api::result_catalog::ResultCatalog;
use houyicoder_api::session::{SessionLog, TrajectoryHead, TrajectoryPage, TrajectoryPosition};
use houyicoder_async::PFut;
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, ContextSnapshot, EventId,
    SessionId, SessionLogEntry,
};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::result_catalog::ResultListQuery;
use houyicoder_session::SessionStore;

use super::{SessionResultCatalog, seed_call};

/// The bytes the gap page reports as passed over.
const GAP_BYTES: u64 = 4096;

/// A log that answers its first page read with a gap: no record, a position
/// the store reached, and the bytes it passed over. A record wider than one
/// read produces the same page shape, and the fold must not read it as the end
/// of the log.
struct GappedLog {
    inner: Arc<dyn SessionLog>,
    gapped: AtomicBool,
}

impl SessionLog for GappedLog {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        self.inner.append(event)
    }

    fn append_if<'a>(
        &'a self,
        event: SessionLogEntry,
        allow: &'a (dyn Fn(&[SessionLogEntry]) -> bool + Send + Sync),
    ) -> PFut<'a, Result<Option<EventId>, ContextError>> {
        self.inner.append_if(event, allow)
    }

    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.inner.replay(session)
    }

    fn current_view(&self, session: SessionId) -> PFut<'_, Result<ContextSnapshot, ContextError>> {
        self.inner.current_view(session)
    }

    fn trajectory_tail(&self, session: SessionId) -> Vec<SessionLogEntry> {
        self.inner.trajectory_tail(session)
    }

    fn resident_base(&self, session: SessionId) -> Result<Option<usize>, ContextError> {
        self.inner.resident_base(session)
    }

    fn trajectory_page(
        &self,
        session: SessionId,
        from: TrajectoryPosition,
        limit: usize,
    ) -> Result<TrajectoryPage, ContextError> {
        if self.gapped.swap(false, Ordering::SeqCst) {
            return Ok(TrajectoryPage {
                events: Vec::new(),
                next: TrajectoryPosition {
                    index: from.index,
                    byte_offset: Some(GAP_BYTES),
                },
                skipped_bytes: GAP_BYTES,
            });
        }
        self.inner.trajectory_page(session, from, limit)
    }

    fn trajectory_head(&self, session: SessionId) -> TrajectoryHead {
        self.inner.trajectory_head(session)
    }

    fn reset_trajectory(&self, session: SessionId) {
        self.inner.reset_trajectory(session);
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

    fn backend(&self) -> &dyn ContextBackend {
        self.inner.backend()
    }
}

/// A page that advances without holding a record is a gap, not the end of the
/// log. The result stored after it is still listed, and the bytes the page
/// passed over are reported rather than swallowed.
#[tokio::test]
async fn test_fold_passes_over_gap() {
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    seed_call(&store, session, "call-1", "bash", b"first", b"");
    seed_call(&store, session, "call-2", "bash", b"second", b"");
    let log: Arc<dyn SessionLog> = Arc::new(GappedLog {
        inner: Arc::clone(&store),
        gapped: AtomicBool::new(true),
    });
    let catalog = SessionResultCatalog::new(log);

    let page = catalog
        .list(session, ResultListQuery::default())
        .await
        .expect("the listing answers");
    let ids: Vec<&str> = page.entries.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        ids,
        ["call-1:stdout", "call-2:stdout"],
        "both calls are listed past the gap: {ids:?}"
    );
    assert!(!page.complete, "a gap leaves the listing partial");

    let stats = catalog.stats(session).await.expect("the aggregate answers");
    assert_eq!(
        stats.skipped_bytes, GAP_BYTES,
        "the bytes the page passed over are reported"
    );
    assert!(!stats.complete, "a gap leaves the aggregate partial");
}
