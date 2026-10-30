//! Tests for the storage trait's default answers.

use houyicoder_async::PFut;

use super::{ContextBackend, ContextError};
use crate::{BlockHash, CheckpointId, CheckpointManifest, EventId, SessionId, SessionLogEntry};

/// A backend that implements only the operations the trait requires, so every
/// optional capability answers with the trait's default.
struct LogOnly;

impl ContextBackend for LogOnly {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        let id = event.id;
        Box::pin(async move { Ok(id) })
    }

    fn read_range(
        &self,
        _session: SessionId,
        _from: Option<EventId>,
        _to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }

    fn replay(&self, _session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }

    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        let id = manifest.id;
        Box::pin(async move { Ok(id) })
    }

    fn read_checkpoint(
        &self,
        _id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        Box::pin(async move { Err(ContextError::NotFound) })
    }

    fn list_checkpoints(
        &self,
        _session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
}

/// A store that implements no windowed read answers Unsupported rather than
/// materializing the whole block, so a reader reports that it cannot page
/// instead of quietly paying the cost paging exists to avoid.
#[test]
fn test_range_read_default_refused() {
    let backend = LogOnly;
    let got = pollster::block_on(backend.block_get_range(&BlockHash("0".repeat(64)), 0, 8));
    assert!(
        matches!(got, Err(ContextError::Unsupported)),
        "a backend without windowed reads answers Unsupported: {got:?}"
    );
}
