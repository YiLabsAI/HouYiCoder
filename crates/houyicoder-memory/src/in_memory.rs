//! InMemoryBackend: the canonical ContextBackend impl and test double. Stores
//! events and checkpoints in HashMaps behind a Mutex. Not for production (no
//! persistence) but exactly tracks the interface a real backend implements.
//! The ContextBackend interface and SessionLogEntry types live in the context layer;
//! this crate depends on that, not the reverse (dependency inversion).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use houyicoder_async::PFut;
use houyicoder_context::{
    BlockHash, CheckpointId, CheckpointManifest, ContextBackend, ContextError, EventId, SessionId,
    SessionLogEntry,
};

use crate::sha256_hex;

#[derive(Default)]
pub struct InMemoryBackend {
    events: Mutex<HashMap<SessionId, Vec<SessionLogEntry>>>,
    seen: Mutex<HashMap<SessionId, HashSet<EventId>>>,
    checkpoints: Mutex<HashMap<CheckpointId, CheckpointManifest>>,
    // Content-addressed block store: hash -> blob. Insert is idempotent so
    // duplicate puts of the same content are a no-op (dedup).
    blocks: Mutex<HashMap<BlockHash, Vec<u8>>>,
}

impl InMemoryBackend {
    /// Construct an empty in-memory backend.
    pub fn new() -> Self {
        Self::default()
    }

    fn append_sync(&self, event: SessionLogEntry) -> Result<EventId, ContextError> {
        let mut events = self.events.lock().expect("events mutex poisoned");
        let mut seen = self.seen.lock().expect("seen mutex poisoned");
        let set = seen.entry(event.session).or_default();
        if set.contains(&event.id) {
            // Main-chain dedup: a duplicate id is a no-op.
            return Ok(event.id);
        }
        set.insert(event.id);
        events.entry(event.session).or_default().push(event.clone());
        Ok(event.id)
    }

    fn read_range_sync(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> Result<Vec<SessionLogEntry>, ContextError> {
        let events = self.events.lock().expect("events mutex poisoned");
        let Some(rows) = events.get(&session) else {
            return Ok(Vec::new());
        };
        let in_range: Vec<SessionLogEntry> = rows
            .iter()
            .filter(|e| from.is_none_or(|f| e.id >= f))
            .filter(|e| to.is_none_or(|t| e.id < t))
            .cloned()
            .collect();
        Ok(in_range)
    }

    fn replay_sync(&self, session: SessionId) -> Result<Vec<SessionLogEntry>, ContextError> {
        let events = self.events.lock().expect("events mutex poisoned");
        Ok(events.get(&session).cloned().unwrap_or_default())
    }

    fn write_checkpoint_sync(
        &self,
        manifest: CheckpointManifest,
    ) -> Result<CheckpointId, ContextError> {
        let id = manifest.id;
        self.checkpoints
            .lock()
            .expect("checkpoints mutex poisoned")
            .insert(id, manifest);
        Ok(id)
    }

    fn read_checkpoint_sync(&self, id: CheckpointId) -> Result<CheckpointManifest, ContextError> {
        self.checkpoints
            .lock()
            .expect("checkpoints mutex poisoned")
            .get(&id)
            .cloned()
            .ok_or(ContextError::NotFound)
    }

    fn list_checkpoints_sync(&self, session: SessionId) -> Result<Vec<CheckpointId>, ContextError> {
        let checkpoints = self.checkpoints.lock().expect("checkpoints mutex poisoned");
        let mut ids: Vec<CheckpointId> = checkpoints
            .values()
            .filter(|m| m.session == session)
            .map(|m| m.id)
            .collect();
        ids.sort();
        Ok(ids)
    }

    fn block_put_sync(&self, block: Vec<u8>) -> Result<BlockHash, ContextError> {
        let hash = sha256_hex(&block);
        let mut blocks = self.blocks.lock().expect("blocks mutex poisoned");
        // Dedup: same hash means same content; do not overwrite.
        blocks.entry(hash.clone()).or_insert(block);
        Ok(hash)
    }

    fn block_get_sync(&self, hash: &BlockHash) -> Result<Vec<u8>, ContextError> {
        let blocks = self.blocks.lock().expect("blocks mutex poisoned");
        blocks.get(hash).cloned().ok_or(ContextError::NotFound)
    }
}

impl ContextBackend for InMemoryBackend {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        let id = self.append_sync(event);
        Box::pin(async move { id })
    }

    fn read_range(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        let out = self.read_range_sync(session, from, to);
        Box::pin(async move { out })
    }

    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        let out = self.replay_sync(session);
        Box::pin(async move { out })
    }

    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        let id = self.write_checkpoint_sync(manifest);
        Box::pin(async move { id })
    }

    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        let out = self.read_checkpoint_sync(id);
        Box::pin(async move { out })
    }

    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        let out = self.list_checkpoints_sync(session);
        Box::pin(async move { out })
    }

    fn block_put(&self, block: Vec<u8>) -> PFut<'_, Result<BlockHash, ContextError>> {
        let out = self.block_put_sync(block);
        Box::pin(async move { out })
    }

    fn block_get(&self, hash: &BlockHash) -> PFut<'_, Result<Vec<u8>, ContextError>> {
        let out = self.block_get_sync(hash);
        Box::pin(async move { out })
    }

    /// Read the full event log synchronously (for the search snapshot). The
    /// in-memory store has no corrupt lines, so this delegates to the same
    /// internal read the async replay path uses. The trait's
    /// read_log_lenient default then returns the events with skipped=0.
    fn read_log(&self, session: SessionId) -> Result<Vec<SessionLogEntry>, ContextError> {
        self.replay_sync(session)
    }
}

#[cfg(test)]
mod tests;
