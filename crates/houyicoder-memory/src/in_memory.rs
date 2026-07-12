//! InMemoryBackend: the canonical ContextBackend impl and test double. Stores
//! events and checkpoints in HashMaps behind a Mutex. Not for production (no
//! persistence) but exactly tracks the interface a real backend implements.
//! The ContextBackend interface and TurnEvent types live in the context layer;
//! this crate depends on that, not the reverse (dependency inversion).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use houyicoder_async::PFut;
use houyicoder_context::{
    BlockHash, CheckpointId, CheckpointManifest, ContextBackend, ContextError, EventId, SessionId,
    TurnEvent,
};

use crate::sha256_hex;

#[derive(Default)]
pub struct InMemoryBackend {
    events: Mutex<HashMap<SessionId, Vec<TurnEvent>>>,
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

    fn append_sync(&self, event: TurnEvent) -> Result<EventId, ContextError> {
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
    ) -> Result<Vec<TurnEvent>, ContextError> {
        let events = self.events.lock().expect("events mutex poisoned");
        let Some(rows) = events.get(&session) else {
            return Ok(Vec::new());
        };
        let in_range: Vec<TurnEvent> = rows
            .iter()
            .filter(|e| from.is_none_or(|f| e.id >= f))
            .filter(|e| to.is_none_or(|t| e.id < t))
            .cloned()
            .collect();
        Ok(in_range)
    }

    fn replay_sync(&self, session: SessionId) -> Result<Vec<TurnEvent>, ContextError> {
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
    fn append(&self, event: TurnEvent) -> PFut<'_, Result<EventId, ContextError>> {
        let id = self.append_sync(event);
        Box::pin(async move { id })
    }

    fn read_range(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<TurnEvent>, ContextError>> {
        let out = self.read_range_sync(session, from, to);
        Box::pin(async move { out })
    }

    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<TurnEvent>, ContextError>> {
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
    fn read_log(&self, session: SessionId) -> Result<Vec<TurnEvent>, ContextError> {
        self.replay_sync(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{Disposition, TurnEvent, TurnEventKind};

    fn event(session: SessionId, id: EventId, kind: TurnEventKind) -> TurnEvent {
        TurnEvent {
            id,
            session,
            ts: 0,
            prev_hash: None,
            kind,
        }
    }

    #[test]
    fn test_in_memory_append_replay() {
        let b = InMemoryBackend::new();
        let s = SessionId::new();
        let e1 = event(
            s,
            EventId::new(),
            TurnEventKind::UserInput { text: "a".into() },
        );
        let e2 = event(
            s,
            EventId::new(),
            TurnEventKind::AssistantMessage {
                text: "b".into(),
                thinking: None,
            },
        );
        pollster::block_on(b.append(e1.clone())).unwrap();
        pollster::block_on(b.append(e2.clone())).unwrap();
        let replay = pollster::block_on(b.replay(s)).unwrap();
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0].id, e1.id);
        assert_eq!(replay[1].id, e2.id);
    }

    #[test]
    fn test_in_memory_dedups_duplicate() {
        let b = InMemoryBackend::new();
        let s = SessionId::new();
        let id = EventId::new();
        let e = event(s, id, TurnEventKind::UserInput { text: "a".into() });
        pollster::block_on(b.append(e.clone())).unwrap();
        pollster::block_on(b.append(e.clone())).unwrap();
        let replay = pollster::block_on(b.replay(s)).unwrap();
        assert_eq!(replay.len(), 1, "duplicate id must be deduped");
    }

    #[test]
    fn test_tool_pair_survives_replay() {
        let b = InMemoryBackend::new();
        let s = SessionId::new();
        let call_id = "toolu_call".to_string();
        pollster::block_on(b.append(event(
            s,
            EventId::new(),
            TurnEventKind::ToolCall {
                call_id: call_id.clone(),
                tool: "edit".into(),
                input: serde_json::json!({}),
            },
        )))
        .unwrap();
        pollster::block_on(b.append(event(
            s,
            EventId::new(),
            TurnEventKind::ToolResult {
                call_id,
                output: serde_json::json!("ok"),
                duration_ms: 0,
            },
        )))
        .unwrap();
        let replay = pollster::block_on(b.replay(s)).unwrap();
        assert_eq!(replay.len(), 2);
        assert!(matches!(replay[0].kind, TurnEventKind::ToolCall { .. }));
        assert!(matches!(replay[1].kind, TurnEventKind::ToolResult { .. }));
    }

    #[test]
    fn test_in_memory_checkpoint_round() {
        let b = InMemoryBackend::new();
        let s = SessionId::new();
        let last = EventId::new();
        let manifest = CheckpointManifest {
            id: CheckpointId::new(),
            session: s,
            last_event: last,
            summary: Some("summarized the head".into()),
            plan: vec![houyicoder_context::TurnGroup {
                turn_id: last,
                disposition: Disposition::Verbatim,
                event_ids: vec![last],
            }],
            ts: 0,
        };
        let id = pollster::block_on(b.write_checkpoint(manifest.clone()));
        assert_eq!(id.unwrap(), manifest.id);
        let back = pollster::block_on(b.read_checkpoint(manifest.id)).unwrap();
        assert_eq!(back.summary, manifest.summary);
        let list = pollster::block_on(b.list_checkpoints(s)).unwrap();
        assert_eq!(list, vec![manifest.id]);
    }

    #[test]
    fn test_block_put_get_roundtrip() {
        let b = InMemoryBackend::new();
        let blob = b"hello cas block store".to_vec();
        let hash = pollster::block_on(b.block_put(blob.clone())).unwrap();
        let back = pollster::block_on(b.block_get(&hash)).unwrap();
        assert_eq!(back, blob);
    }

    #[test]
    fn test_block_put_dedup_same() {
        let b = InMemoryBackend::new();
        let blob = b"dedup me".to_vec();
        let h1 = pollster::block_on(b.block_put(blob.clone())).unwrap();
        let h2 = pollster::block_on(b.block_put(blob.clone())).unwrap();
        assert_eq!(h1, h2, "same content must yield same hash");
        // Retrieval still works after duplicate put.
        let back = pollster::block_on(b.block_get(&h1)).unwrap();
        assert_eq!(back, blob);
    }

    #[test]
    fn test_block_put_different_content() {
        let b = InMemoryBackend::new();
        let h1 = pollster::block_on(b.block_put(b"one".to_vec())).unwrap();
        let h2 = pollster::block_on(b.block_put(b"two".to_vec())).unwrap();
        assert_ne!(h1, h2, "different content must yield different hash");
    }

    #[test]
    fn test_block_get_missing() {
        let b = InMemoryBackend::new();
        let res = pollster::block_on(b.block_get(&BlockHash("0".repeat(64))));
        assert!(matches!(res, Err(ContextError::NotFound)));
    }

    /// Regression for the search snapshot: read_log must return all events
    /// for a session so the search view's snapshot seam can load the full
    /// transcript. Before the fix, InMemoryBackend did not implement
    /// read_log, so the trait default returned Err(Unsupported) -> the
    /// lenient read returned empty -> /search found nothing. The lenient
    /// read delegates to read_log (no corrupt lines in memory).
    #[test]
    fn test_read_log_returns_events() {
        let b = InMemoryBackend::new();
        let s = SessionId::new();
        // Cold session: empty.
        assert!(b.read_log(s).unwrap().is_empty());
        // Append two events.
        pollster::block_on(b.append(event(
            s,
            EventId::new(),
            TurnEventKind::UserInput {
                text: "hello".into(),
            },
        )))
        .unwrap();
        pollster::block_on(b.append(event(
            s,
            EventId::new(),
            TurnEventKind::UserInput {
                text: "world".into(),
            },
        )))
        .unwrap();
        // read_log returns all events in append order.
        let events = b.read_log(s).unwrap();
        assert_eq!(events.len(), 2, "read_log returns all appended events");
        // read_log_lenient delegates to read_log (default: Ok -> events, 0 skipped).
        let lenient = b.read_log_lenient(s);
        assert_eq!(
            lenient.events.len(),
            2,
            "lenient read returns the same events"
        );
        assert_eq!(
            lenient.skipped, 0,
            "no corrupt lines in memory -> 0 skipped"
        );
    }

    #[test]
    fn test_block_large_blob_externalized() {
        // A large blob goes to the CAS, not the event log: events stay empty.
        let b = InMemoryBackend::new();
        let big = vec![0xABu8; 4096];
        let hash = pollster::block_on(b.block_put(big.clone())).unwrap();
        let back = pollster::block_on(b.block_get(&hash)).unwrap();
        assert_eq!(back, big);
        let events = pollster::block_on(b.replay(SessionId::new())).unwrap();
        assert!(events.is_empty(), "CAS blob must not land in event log");
    }

    #[test]
    fn test_context_backend_is_object() {
        // A future method addition must not silently break runtime dispatch.
        let _boxed: Box<dyn ContextBackend> = Box::new(InMemoryBackend::new());
    }
}
