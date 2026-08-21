//! The event-driven session layer.
//!
//! SessionStore is the engine-facing facade over a ContextBackend: it appends
//! TurnEvents with a tamper-evident hash-chain, tracks a delta-persistence
//! counter for interrupted-turn rewind, and assembles the served context view
//! by applying a CompactionPlan to a replay. The raw log (owned by the
//! ContextBackend in the context layer) is never mutated; compaction is
//! view-selection, not destruction.
//!
//! Disentangled: ContextBackend + TurnEvent + CompactionPlan live in the
//! context layer (this crate depends on that, not the reverse). Backends
//! live in the memory layer. The agent loop (the engine) drives a
//! SessionStore.
//!
//! Hash-chain: each event's prev_hash is SHA-256 of the canonical JSON of the
//! previous event (including that event's own prev_hash), so tampering with
//! event N breaks the link at event N+1. The chain is computed here on append
//! and cached (O(1) per append); the backend stores it verbatim. A future
//! verify_chain walk detects tampering on replay (not yet implemented).
//!
//! Delta-persistence counter: an absolute "how many items flushed"
//! pointer, set (overwritten) after each turn's flush; rewind(n) backs it on
//! interrupted-turn resume so the pending tool output still gets written. The
//! counter is in-memory only; the event log is lossless (rewind never drops
//! events). Non-optional in this engine.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use houyicoder_async::PFut;
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, ContextSnapshot, EventId,
    PrevHash, SessionId, TurnEvent, TurnEventKind,
};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

/// The engine-facing session facade. Owns a ContextBackend and layers the
/// hash-chain, delta counter, and view assembly on top. Construct with any
/// backend (InMemory for tests, LocalFile for persistence, future sqlite/cloud).
pub struct SessionStore {
    backend: Box<dyn ContextBackend>,
    /// Cache of the last event's hash per session, so append is O(1) instead
    /// of replaying the full log per append. Single-process: if the backend is
    /// mutated out-of-band, this cache goes stale (clear with reload).
    last_hashes: Mutex<HashMap<SessionId, PrevHash>>,
    /// Serializes appends (compute prev_hash + backend append + cache update)
    /// so concurrent same-session appends don't fork the chain. v0: a single
    /// lock serializes all sessions; a real impl uses per-session locks.
    /// tokio::sync::Mutex so the guard is Send across the backend awaits.
    append_lock: tokio::sync::Mutex<()>,
    /// Per-session delta-persistence counter (the interrupted-turn rewind
    /// pattern).
    persisted: Mutex<HashMap<SessionId, u32>>,
    /// Per-session in-memory mirror of finalized events (prev_hash set), the
    /// /trajectory command's sync substrate. The raw append-only log in the
    /// backend stays the source of truth; this mirror lets /trajectory read
    /// without an async replay. A resumed session starts with an empty mirror
    /// until restore_trajectory backfills it from the durable log.
    trajectory: Mutex<HashMap<SessionId, Vec<TurnEvent>>>,
    /// Optional Notify fired on each append so a host draining mid-run wakes
    /// to push the new durable event without waiting for the run future to
    /// resolve. None when no host wires the mid-run drain; behavior is then
    /// today's (events push only at run resolve). Shared by Arc with the host
    /// at the composition root.
    append_notify: Option<Arc<Notify>>,
}

/// Outcome of verifying an imported session's source chain. The chain is
/// checked against the re-serialized line bytes (the same form the exporting
/// binary hashed). A schema change between the exporter and the importer can
/// make re-serialization differ, so verification is best-effort: Unverified
/// is recorded in provenance and the import still proceeds with a rebuilt
/// durable chain (which is internally consistent by construction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceChain {
    /// Every event's prev_hash chains to the previous event's line-byte hash.
    Verified,
    /// The chain broke at the given index for the given reason. The import
    /// continues with a rebuilt durable chain; only the source is marked
    /// unverified.
    Unverified { at_index: usize, reason: String },
}

/// Report from seeding a session store from an imported trajectory. Carries
/// the rebuilt durable chain's head hash (for meta + provenance), the count of
/// durable events written, the count of streaming deltas dropped (they are
/// transport-only, never persisted), and the source-chain verdict. This is a
/// typed report, not an error: an unverified source is a warning, not a
/// failure, so the rescue path never hard-fails on the one file it exists to
/// save.
#[derive(Debug, Clone)]
pub struct ImportReport {
    pub durable_count: usize,
    pub deltas_dropped: usize,
    pub source_chain: SourceChain,
    pub head_hash: Option<PrevHash>,
}

impl SessionStore {
    /// Construct a session store over any ContextBackend.
    pub fn new(backend: Box<dyn ContextBackend>) -> Self {
        Self {
            backend,
            last_hashes: Mutex::new(HashMap::new()),
            append_lock: tokio::sync::Mutex::new(()),
            persisted: Mutex::new(HashMap::new()),
            trajectory: Mutex::new(HashMap::new()),
            append_notify: None,
        }
    }

    /// Share an Append Notify so a host draining mid-run wakes on each
    /// append. The host holds its own Arc clone; this store fires notify_one
    /// per append. Composable with new (defaults to None — today's behavior,
    /// events push only at run resolve).
    pub fn with_append_notify(mut self, notify: Arc<Notify>) -> Self {
        self.append_notify = Some(notify);
        self
    }

    /// Borrow the underlying backend for CAS operations (block_put / block_get)
    /// from the projection layer without owning the store.
    pub fn backend(&self) -> &dyn ContextBackend {
        &*self.backend
    }

    /// Append an event. Streaming text deltas (AssistantTextDelta) take a
    /// separate path from durable events: a delta is a process-local
    /// transport record for live display, not a durable audit row. It enters
    /// only the in-memory mirror and fires the notify (so a host draining
    /// mid-run sees the streamed text), but it does NOT enter the backend log,
    /// the hash chain, or last_hashes. The authoritative AssistantMessage
    /// (full text) lands at turn end and carries the durable record; the
    /// delta is its strict sub-event for display only. The transcript
    /// stores complete assistant messages, not per-token rows -- deltas
    /// in the durable log were 84% of events with zero replay value.
    ///
    /// Durable events keep the existing path: compute prev_hash over the
    /// durable subset (deltas never touched last_hashes, so the chain skips
    /// them naturally), hash, persist to the backend, cache the new hash,
    /// mirror, notify.
    pub async fn append(&self, mut event: TurnEvent) -> Result<EventId, ContextError> {
        let _guard = self.append_lock.lock().await;
        let session = event.session;
        if matches!(event.kind, TurnEventKind::AssistantTextDelta { .. }) {
            // Delta path: mirror + notify only. No backend, no chain, no cache.
            self.trajectory
                .lock()
                .expect("trajectory mutex poisoned")
                .entry(session)
                .or_default()
                .push(event.clone());
            if let Some(n) = &self.append_notify {
                n.notify_waiters();
            }
            return Ok(event.id);
        }
        // Durable path.
        let prev_hash = self.compute_prev_hash(session).await?;
        event.prev_hash = prev_hash;
        let finalized = event.clone();
        let new_hash = Self::hash_event(&event)?;
        let id = self.backend.append(event).await?;
        self.last_hashes
            .lock()
            .expect("last_hashes mutex poisoned")
            .insert(session, new_hash);
        self.trajectory
            .lock()
            .expect("trajectory mutex poisoned")
            .entry(session)
            .or_default()
            .push(finalized);
        // Wake a mid-run draining host (route B) so the new durable event
        // pushes without waiting for the run future to resolve.
        // notify_waiters (not notify_one): notify_one stores a permit when no
        // waiter is parked, so the next notified() returns immediately --
        // which causes a busy-loop (push_new_events → loop → notified()
        // ready → push_new_events → ...) that starves the select's
        // io.next_frame branch (where AbortRun arrives). notify_waiters
        // drops the wake when no waiter is parked; the new event is caught
        // by the next append's wake or the post-run drain (push_new_events
        // re-scans from a cursor, so a lost wake does not lose data).
        if let Some(n) = &self.append_notify {
            n.notify_waiters();
        }
        Ok(id)
    }

    /// Read the in-memory trajectory mirror for a session (sync): the finalized
    /// events in append order, each with prev_hash set. Empty when no events
    /// have been appended this process for the session. The /trajectory command
    /// projects this directly — TurnEvent already carries the id, ts, prev_hash,
    /// and kind a trajectory row needs, so no separate record type is introduced.
    pub fn trajectory_snapshot(&self, session: SessionId) -> Vec<TurnEvent> {
        self.trajectory
            .lock()
            .expect("trajectory mutex poisoned")
            .get(&session)
            .cloned()
            .unwrap_or_default()
    }

    /// Drop the in-memory trajectory mirror for a session (the /clear path).
    /// The backend's append-only log is untouched — this only frees the
    /// viewable mirror so /trajectory reads fresh after a clear.
    pub fn reset_trajectory(&self, session: SessionId) {
        self.trajectory
            .lock()
            .expect("trajectory mutex poisoned")
            .remove(&session);
    }

    /// Backfill the in-memory trajectory mirror from the backend log for a
    /// resumed session (read-only — no append, no chain rewrite). The mirror
    /// starts empty on resume; this loads the durable history so the
    /// serve-start replay ships the resumed conversation to the client (the
    /// working screen shows the past turns, not just the status bar). Also
    /// primes last_hashes so the next append chains from the real last event
    /// instead of falling through to a backend replay. Returns the event
    /// count loaded.
    pub async fn restore_trajectory(&self, session: SessionId) -> Result<usize, ContextError> {
        let events = self.backend.replay(session).await?;
        let count = events.len();
        if count == 0 {
            return Ok(0);
        }
        {
            let mut traj = self.trajectory.lock().expect("trajectory mutex poisoned");
            traj.insert(session, events.clone());
        }
        if let Some(last) = events.last() {
            let h = Self::hash_event(last)?;
            self.last_hashes
                .lock()
                .expect("last_hashes mutex poisoned")
                .insert(session, h);
        }
        Ok(count)
    }

    /// Compute the prev_hash for the next event: the cached hash of the last
    /// event, or (cache miss) the hash of the backend's last event. None if
    /// the session is empty.
    async fn compute_prev_hash(
        &self,
        session: SessionId,
    ) -> Result<Option<PrevHash>, ContextError> {
        if let Some(h) = self
            .last_hashes
            .lock()
            .expect("last_hashes mutex poisoned")
            .get(&session)
            .copied()
        {
            return Ok(Some(h));
        }
        let events = self.backend.replay(session).await?;
        let Some(last) = events.last() else {
            return Ok(None);
        };
        let h = Self::hash_event(last)?;
        self.last_hashes
            .lock()
            .expect("last_hashes mutex poisoned")
            .insert(session, h);
        Ok(Some(h))
    }

    /// SHA-256 of the canonical JSON of an event (including its own prev_hash).
    fn hash_event(event: &TurnEvent) -> Result<PrevHash, ContextError> {
        let bytes = serde_json::to_vec(event)
            .map_err(|_| ContextError::Corrupt("event failed to serialize".into()))?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let hash: [u8; 32] = hasher
            .finalize()
            .as_slice()
            .try_into()
            .expect("sha256 is 32 bytes");
        Ok(PrevHash(hash))
    }

    /// SHA-256 of raw line bytes (the on-disk line without trailing newline).
    /// The verify path hashes the bytes as-read, never re-serializing the
    /// parsed struct, so a schema change (a new serde-default field) does not
    /// break old logs: the chain is byte-stable, not schema-stable. The write
    /// path keeps hash_event (to_vec) because within one binary to_vec equals
    /// the disk line bytes, so last_hashes stays consistent.
    fn hash_line_bytes(bytes: &[u8]) -> PrevHash {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let hash: [u8; 32] = hasher
            .finalize()
            .as_slice()
            .try_into()
            .expect("sha256 is 32 bytes");
        PrevHash(hash)
    }

    /// Seed the store from an imported trajectory (an export or an old log
    /// replayed elsewhere). Two-step: (1) verify the source chain as the
    /// exporting binary wrote it -- re-serialize each event to line bytes and
    /// check prev_hash linkage, including deltas. A schema change between
    /// exporter and importer can make this mismatch, so the verdict is
    /// best-effort and recorded, not fatal. (2) Rebuild the durable subset
    /// chain by appending each durable event through the normal append path,
    /// which recomputes prev_hash over the durable subset (deltas never
    /// entered last_hashes, so the chain skips them naturally). Historical
    /// deltas are dropped entirely -- they are transport-only, never
    /// persisted, never mirrored (their authoritative AssistantMessage is
    /// the full superset). Returns a typed report; an unverified source is a
    /// warning, not an error, so the rescue never hard-fails here.
    pub async fn seed_trajectory(
        &self,
        session: SessionId,
        events: Vec<TurnEvent>,
    ) -> Result<ImportReport, ContextError> {
        // Step 1: verify the source chain (read-only, before consuming).
        let source_chain = Self::verify_source_chain(&events);
        // Step 2: rebuild the durable subset. Each append takes the
        // append_lock (uncontended during seed -- the runner is not driving
        // yet), recomputes prev_hash from last_hashes (durable-only), and
        // persists to the backend. Deltas are dropped, not appended.
        let mut durable_count = 0usize;
        let mut deltas_dropped = 0usize;
        for mut ev in events {
            if matches!(ev.kind, TurnEventKind::AssistantTextDelta { .. }) {
                deltas_dropped += 1;
                continue;
            }
            // Rewrite the event onto the destination session (import forks a
            // new session; the source events carry the original sid).
            ev.session = session;
            self.append(ev).await?;
            durable_count += 1;
        }
        let head_hash = self
            .last_hashes
            .lock()
            .expect("last_hashes mutex poisoned")
            .get(&session)
            .copied();
        Ok(ImportReport {
            durable_count,
            deltas_dropped,
            source_chain,
            head_hash,
        })
    }

    /// Verify a source event list's prev_hash chain by re-serializing each
    /// event to compact line bytes (the form the exporting binary hashed) and
    /// checking linkage. Genesis (first event) must have prev_hash None. A
    /// serialize failure or a mismatch yields Unverified with the index and
    /// reason; the caller proceeds with a rebuilt chain regardless.
    fn verify_source_chain(events: &[TurnEvent]) -> SourceChain {
        let mut prev: Option<PrevHash> = None;
        for (i, ev) in events.iter().enumerate() {
            let Ok(bytes) = serde_json::to_vec(ev) else {
                return SourceChain::Unverified {
                    at_index: i,
                    reason: "event failed to serialize".into(),
                };
            };
            let h = Self::hash_line_bytes(&bytes);
            if ev.prev_hash != prev {
                return SourceChain::Unverified {
                    at_index: i,
                    reason: "prev_hash does not chain to the previous event".into(),
                };
            }
            prev = Some(h);
        }
        SourceChain::Verified
    }

    /// Verify a disk log's prev_hash chain by hashing the RAW line bytes
    /// (as-read via read_log_range), not a re-serialization of the parsed
    /// struct. This is the #15 fix: a schema change that adds a serde-default
    /// field makes re-serialization drift from the on-disk bytes a prior
    /// binary wrote, so a verify that re-serializes would false-positive a
    /// cross-binary log as Unverified. Hashing the raw line bytes is
    /// byte-stable across schema changes (the bytes the writer hashed at
    /// write time are the bytes we hash at verify time). Genesis (first
    /// event) must have prev_hash None. Best-effort: a parse failure or a
    /// mismatch yields Unverified with the index + reason; the caller warns
    /// but does not block resume (a crashed session may still recover).
    pub fn verify_disk_chain(&self, session: SessionId) -> SourceChain {
        let mut lines: Vec<String> = Vec::new();
        let mut offset = 0u64;
        loop {
            let r = self.backend.read_log_range(session, offset, 64_000);
            if r.lines.is_empty() {
                break;
            }
            for (_off, text) in r.lines {
                lines.push(text);
            }
            if r.next_offset <= offset || offset >= r.bytes_total {
                break;
            }
            offset = r.next_offset;
        }
        let mut prev: Option<PrevHash> = None;
        for (i, line) in lines.iter().enumerate() {
            // Parse only to read prev_hash; a schema drift with serde-default
            // fields still parses. The chain check hashes the RAW line bytes.
            let Ok(ev) = serde_json::from_str::<TurnEvent>(line) else {
                return SourceChain::Unverified {
                    at_index: i,
                    reason: "line failed to parse".into(),
                };
            };
            let h = Self::hash_line_bytes(line.as_bytes());
            if ev.prev_hash != prev {
                return SourceChain::Unverified {
                    at_index: i,
                    reason: "prev_hash does not chain to the previous line's bytes".into(),
                };
            }
            prev = Some(h);
        }
        SourceChain::Verified
    }

    pub async fn replay(&self, session: SessionId) -> Result<Vec<TurnEvent>, ContextError> {
        self.backend.replay(session).await
    }

    /// Persist a compaction manifest (checkpoint). Delegates to the backend;
    /// the manifest is stored separately from the event log and does not
    /// affect the hash chain. The caller (compress runtime) appends
    /// CompactionBoundary + Summary events via append() to record where
    /// compaction happened in the log.
    pub async fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> Result<CheckpointId, ContextError> {
        self.backend.write_checkpoint(manifest).await
    }

    /// Read a checkpoint manifest by id. Used for round-trip verification and
    /// /rewind picker construction.
    pub async fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> Result<CheckpointManifest, ContextError> {
        self.backend.read_checkpoint(id).await
    }

    /// List checkpoint ids for a session, oldest first.
    pub async fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> Result<Vec<CheckpointId>, ContextError> {
        self.backend.list_checkpoints(session).await
    }

    /// Assemble the served context view. Reads the full event log and, if a
    /// checkpoint exists, loads the latest manifest so the caller can apply the
    /// per-event Disposition plan (Verbatim / Summarized / Referenced). No
    /// checkpoint means full replay (no plan applied).
    pub async fn current_view(&self, session: SessionId) -> Result<ContextSnapshot, ContextError> {
        let events = self.backend.replay(session).await?;
        let rewind_points = self.backend.list_checkpoints(session).await?;
        let last_checkpoint = rewind_points.last().copied();
        let manifest = if let Some(cp) = last_checkpoint {
            Some(self.backend.read_checkpoint(cp).await?)
        } else {
            None
        };
        Ok(ContextSnapshot {
            session,
            events,
            last_checkpoint,
            rewind_points,
            manifest,
        })
    }

    /// Record the absolute count of generated items flushed for this session
    /// (delta-persistence counter). Overwrites the prior count — the caller
    /// passes the cumulative flushed count after each turn's flush.
    pub fn mark_persisted(&self, session: SessionId, count: u32) {
        self.persisted
            .lock()
            .expect("persisted mutex poisoned")
            .insert(session, count);
    }

    /// Rewind the persisted-count by n (saturating at 0). Called on
    /// interrupted-turn resume so the pending tool output still gets written.
    /// This adjusts the in-memory flush pointer only; the event log is
    /// lossless (no events are dropped). Returns the new count, or None if the
    /// session has no counter yet.
    pub fn rewind_persisted(&self, session: SessionId, n: u32) -> Option<u32> {
        let mut persisted = self.persisted.lock().expect("persisted mutex poisoned");
        let count = persisted.get_mut(&session)?;
        *count = count.saturating_sub(n);
        Some(*count)
    }
}

/// The port impl: SessionStore satisfies the engine-facing SessionLog trait by
/// delegating to its inherent methods. Boxed futures re-pin the async bodies so
/// the trait stays object-safe (Arc<dyn SessionLog>).
impl houyicoder_api::session::SessionLog for SessionStore {
    fn append(&self, event: TurnEvent) -> PFut<'_, Result<EventId, ContextError>> {
        Box::pin(Self::append(self, event))
    }
    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<TurnEvent>, ContextError>> {
        Box::pin(Self::replay(self, session))
    }
    fn current_view(&self, session: SessionId) -> PFut<'_, Result<ContextSnapshot, ContextError>> {
        Box::pin(Self::current_view(self, session))
    }
    fn trajectory_snapshot(&self, session: SessionId) -> Vec<TurnEvent> {
        Self::trajectory_snapshot(self, session)
    }
    fn reset_trajectory(&self, session: SessionId) {
        Self::reset_trajectory(self, session);
    }
    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        Box::pin(Self::write_checkpoint(self, manifest))
    }
    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        Box::pin(Self::read_checkpoint(self, id))
    }
    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        Box::pin(Self::list_checkpoints(self, session))
    }
    fn backend(&self) -> &dyn ContextBackend {
        Self::backend(self)
    }
    fn session_log_root(&self) -> Option<std::path::PathBuf> {
        self.backend
            .session_log_root()
            .map(std::path::Path::to_path_buf)
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
