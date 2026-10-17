//! The event-driven session layer.
//!
//! SessionStore is the engine-facing facade over a ContextBackend: it appends
//! SessionLogEntries with a tamper-evident hash-chain, tracks a delta-persistence
//! counter for interrupted-turn rewind, and assembles the context by
//! applying a CompactionPlan to a replay. The raw log (owned by the
//! ContextBackend in the context layer) is never mutated; compaction is
//! view-selection, not destruction.
//!
//! Disentangled: ContextBackend + SessionLogEntry + CompactionPlan live in the
//! context layer (this crate depends on that, not the reverse). Backends
//! live in the memory layer. The agent loop (the engine) drives a
//! SessionStore.
//!
//! Hash-chain: each event's prev_hash is SHA-256 of the previous event's line
//! bytes, so tampering with event N breaks the link event N+1 carries. The
//! chain is computed here on append and cached (O(1) per append); the backend
//! stores it verbatim. Verifying the links a log or an import already carries
//! lives in the chain module, over the same hash.
//!
//! Delta-persistence counter: an absolute "how many items flushed"
//! pointer, set (overwritten) after each turn's flush; rewind(n) backs it on
//! interrupted-turn resume so the pending tool output still gets written. The
//! counter is in-memory only; the event log is lossless (rewind never drops
//! events). Non-optional in this engine.

use std::collections::{HashMap, HashSet};
use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use houyicoder_api::session::{SessionLog, TrajectoryHead, TrajectoryPage, TrajectoryPosition};
use houyicoder_async::PFut;
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, ContextSnapshot, EventId,
    PrevHash, SessionEvent, SessionId, SessionLogEntry,
};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Notify;

mod chain;
mod log_windows;
mod trajectory;

use chain::LogChain;
use trajectory::{LogStates, RestoredHistory, WindowRead};

/// The index the session's current epoch begins at: the last clear boundary,
/// or the log head when the session was never cleared. The durable log keeps
/// the archived turns so /resume can read them back, so a reader that
/// describes the live session starts at the boundary rather than at the head.
/// The boundary event itself is included. Callers that read the log whole
/// (full-text search, memory preservation) use replay instead.
fn epoch_start(events: &[SessionLogEntry]) -> usize {
    events
        .iter()
        .rposition(|entry| matches!(entry.event, SessionEvent::ContextCleared { .. }))
        .unwrap_or(0)
}

/// The bytes a session's log state may hold, from the test variable when one is
/// set and from the compiled default otherwise.
fn resident_budget_from_env() -> usize {
    budget_from_setting(env::var(trajectory::RESIDENT_BUDGET_ENV).ok())
}

/// The budget a raw variable value names. A value that is not a byte count is
/// reported and ignored; any count it names is kept, zero included, because
/// the window holds its newest row at any budget, so a zero window costs the
/// history behind that row rather than the row itself.
fn budget_from_setting(setting: Option<String>) -> usize {
    let Some(setting) = setting else {
        return trajectory::DEFAULT_RESIDENT_BYTES;
    };
    setting.parse().unwrap_or_else(|_| {
        tracing::warn!(
            "{} is not a byte count: {setting}",
            trajectory::RESIDENT_BUDGET_ENV
        );
        trajectory::DEFAULT_RESIDENT_BYTES
    })
}

/// The engine-facing session facade. Owns a ContextBackend and layers the
/// hash-chain, delta counter, and view assembly on top. Construct with any
/// backend (InMemory for tests, LocalFile for persistence, future sqlite/cloud).
pub struct SessionStore {
    backend: Box<dyn ContextBackend>,
    /// The hash each session's newest durable record carries, so an append
    /// links the record it follows without reading the log back.
    chain: LogChain,
    /// Serializes appends (compute prev_hash + backend append + cache update)
    /// so concurrent same-session appends don't fork the chain. v0: a single
    /// lock serializes all sessions; a real impl uses per-session locks.
    /// tokio::sync::Mutex so the guard is Send across the backend awaits.
    append_lock: AsyncMutex<()>,
    /// Per-session delta-persistence counter (the interrupted-turn rewind
    /// pattern).
    persisted: Mutex<HashMap<SessionId, u32>>,
    /// Per-session in-memory state for finalized events and their delegated
    /// usage. The raw append-only log stays the source of truth; this gives
    /// trajectory a sync read and status a constant-time aggregate. Resume
    /// backfills both from the same durable revision.
    states: LogStates,
    /// Optional Notify fired on each append so a host draining mid-run wakes
    /// to push the new durable event without waiting for the run future to
    /// resolve. None when no host wires the mid-run drain; behavior is then
    /// today's (events push only at run resolve). Shared by Arc with the host
    /// at the composition root.
    append_notify: Option<Arc<Notify>>,
    /// One-shot hook fired the first time a DURABLE event lands for a
    /// session (deltas do not trigger it). The composition root installs it
    /// so a fresh session's descriptor - which carries the model/cwd/provenance
    /// a resume needs - is written only once the session has real content,
    /// not at build time. A build that never runs a turn leaves no descriptor,
    /// no directory, no orphan. Resume/fork paths do not install it: their
    /// descriptor already exists on disk, so they write it directly.
    first_durable: Option<Arc<dyn Fn(SessionId) + Send + Sync>>,
    /// Sessions whose first-durable hook has already fired. Guards the
    /// one-shot contract across appends to the same store.
    first_durable_fired: Mutex<HashSet<SessionId>>,
}

/// The verdict on a chain of records: whether every link holds.
///
/// A record's prev_hash is the hash of the record before it, so a chain is
/// Verified when each record carries the hash of its predecessor, and the
/// index a broken one reports is where the first record that does not.
/// Reached over a log's line bytes and over an imported event list, so a
/// writer and a reader of either agree on what a broken chain is. Best-effort
/// by design: a schema change between the two can make a re-serialized list
/// differ, so an unverified chain is recorded and the caller rebuilds one,
/// which is consistent by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainVerdict {
    /// Every record's prev_hash chains to the record before it.
    Verified,
    /// The chain broke at the given index for the given reason.
    Unverified { at_index: usize, reason: String },
}

/// Report from seeding a session store from an imported trajectory. Carries
/// the rebuilt durable chain's head hash (for descriptor + provenance), the
/// count of durable events written, the count of streaming deltas dropped
/// (they are transport-only, never persisted), and the verdict on the source
/// chain. This is a typed report, not an error: an unverified source is a
/// warning, not a failure, so the rescue path never hard-fails on the one file
/// it exists to save.
#[derive(Debug, Clone)]
pub struct ImportReport {
    pub durable_count: usize,
    pub deltas_dropped: usize,
    pub source_chain: ChainVerdict,
    pub head_hash: Option<PrevHash>,
}

impl SessionStore {
    /// Construct a session store over any ContextBackend.
    ///
    /// The log state keeps a window of the session's events only over a backend
    /// that can answer a read below it from its own log; a backend with no byte
    /// log has nowhere else to read, so its log state holds the history. The
    /// window's byte budget comes from the test variable when it is set and
    /// from the compiled default otherwise.
    pub fn new(backend: Box<dyn ContextBackend>) -> Self {
        Self::with_resident_budget(backend, resident_budget_from_env())
    }

    /// A store whose log state window holds at most budget_bytes of a session's
    /// rows. The compiled default reaches this through new; a test states the
    /// budget it wants to observe instead, because the variable new reads is
    /// process-wide and every store in the same test process would share it.
    fn with_resident_budget(backend: Box<dyn ContextBackend>, budget_bytes: usize) -> Self {
        let states = LogStates::new(backend.supports_log_windows(), budget_bytes);
        Self {
            backend,
            chain: LogChain::default(),
            append_lock: AsyncMutex::new(()),
            persisted: Mutex::new(HashMap::new()),
            states,
            append_notify: None,
            first_durable: None,
            first_durable_fired: Mutex::new(HashSet::new()),
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

    /// Install a one-shot hook fired on the first durable append for a
    /// session. Used to materialize a fresh session's descriptor only once it
    /// has real content, so a build that never runs a turn leaves nothing
    /// on disk. The hook receives the session the durable event is for.
    pub fn with_first_durable(mut self, hook: Arc<dyn Fn(SessionId) + Send + Sync>) -> Self {
        self.first_durable = Some(hook);
        self
    }

    /// Borrow the underlying backend for CAS operations (block_put / block_get)
    /// through the log without owning the store.
    pub fn backend(&self) -> &dyn ContextBackend {
        &*self.backend
    }

    /// Append an event. Streaming text deltas (AssistantTextDelta) take a
    /// separate path from durable events: a delta is a process-local
    /// transport record for live display, not a durable audit row. It enters
    /// only the in-memory log state and fires the notify (so a host draining
    /// mid-run sees the streamed text), but it does NOT enter the backend log,
    /// the hash chain, or the chain's cache. The authoritative AssistantMessage
    /// (full text) lands at turn end and carries the durable record; the
    /// delta is its strict sub-event for display only. The transcript
    /// stores complete assistant messages, not per-token rows -- deltas
    /// in the durable log were 84% of events with zero replay value.
    ///
    /// Durable events keep the existing path: compute prev_hash over the
    /// durable subset (deltas never touch the chain's cache, so the chain
    /// skips them naturally), hash, persist to the backend, cache the new
    /// hash, log state, notify.
    pub async fn append(&self, mut event: SessionLogEntry) -> Result<EventId, ContextError> {
        let _guard = self.append_lock.lock().await;
        let session = event.session;
        if matches!(event.event, SessionEvent::AssistantTextDelta { .. }) {
            // Delta path: log state + notify only. No backend, no chain, no cache.
            let id = event.id;
            // A delta carries no durable fact, so it moves no counter. It does
            // extend the session's span, which is what the old whole-log fold
            // reported while a generation was still streaming.
            self.states.note_delta(session, event);
            if let Some(n) = &self.append_notify {
                n.notify_one();
            }
            return Ok(id);
        }
        // Durable path.
        if self.states.needs_epoch_offset(session) {
            // This record opens the session's epoch, either because it is the
            // session's first durable event or because the log state was dropped
            // for a clear. Where the log stands now is where that epoch starts,
            // and it is the one position the log state cannot recover from the
            // records it later holds.
            self.states
                .start_epoch(session, self.backend.log_size(session));
        }
        let prev_hash = self.chain.link(self.backend.as_ref(), session).await?;
        event.prev_hash = prev_hash;
        let finalized = event.clone();
        let new_hash = chain::hash_event(&event)?;
        let id = self.backend.append(event).await?;
        // First durable event for a session materializes its descriptor. Fired
        // here, after the backend accepted the event, so the session dir the
        // descriptor lands in already exists on a disk backend. Best-effort by
        // contract (the hook swallows write errors): a missing descriptor
        // degrades resume, not the running turn.
        if let Some(hook) = &self.first_durable {
            let mut fired = self.first_durable_fired.lock().expect("first-durable set");
            if !fired.contains(&session) {
                fired.insert(session);
                drop(fired);
                hook(session);
            }
        }
        self.chain.remember(session, new_hash);
        self.states.note_durable(session, finalized);
        // Retain one wake permit when the host is polling the run future, so
        // the durable event cannot remain invisible until the run completes.
        // Notify coalesces surplus permits; draining from a cursor publishes
        // every event covered by that wake without a loop per append.
        if let Some(n) = &self.append_notify {
            n.notify_one();
        }
        Ok(id)
    }

    /// The newest rows of a session's in-memory log state (sync), each
    /// with prev_hash set. Empty when no events have been appended for the
    /// session this process and none were folded in from its log. The
    /// /trajectory command reads this directly — SessionLogEntry already
    /// carries the id, ts, prev_hash, and kind a trajectory row needs, so no
    /// separate record type is introduced.
    ///
    /// This is a tail, not the history: a store over a backend that keeps a log
    /// holds only its newest rows. A reader that must see every record pages
    /// with trajectory_page.
    pub fn trajectory_tail(&self, session: SessionId) -> Vec<SessionLogEntry> {
        self.states.tail(session)
    }

    /// Read one bounded page of a session's durable history from an absolute
    /// position, so a reader can walk a history larger than memory.
    ///
    /// The resident window answers a position it covers; below that the
    /// backend's log does. A below-window position carrying no byte offset is
    /// resolved by reading from where the epoch begins, which costs one parse
    /// per record passed — prefer carrying the offset a page returned. A page
    /// the log cannot fill because the walk reached the window over a record it
    /// could not read is not the page's end: the position it reports is asked
    /// of the window again, which is where those records are.
    ///
    /// With no log state in this process the store folds the session's log first,
    /// because a position is meaningful only against an epoch: its ordinals
    /// start at the newest clear boundary, which nothing but a read of the log
    /// can find.
    pub fn trajectory_page(
        &self,
        session: SessionId,
        from: TrajectoryPosition,
        limit: usize,
    ) -> Result<TrajectoryPage, ContextError> {
        let mut from = from;
        let mut passed: u64 = 0;
        let mut page = loop {
            match self.states.page(session, from, limit) {
                WindowRead::Held { events, next } => {
                    break TrajectoryPage {
                        events,
                        next: TrajectoryPosition {
                            index: next,
                            byte_offset: None,
                        },
                        skipped_bytes: 0,
                    };
                }
                WindowRead::Below {
                    base_index,
                    epoch_byte_offset,
                } => {
                    let read = log_windows::read_page(
                        self.backend.as_ref(),
                        session,
                        from,
                        limit,
                        base_index,
                        epoch_byte_offset,
                    )?;
                    if read.events.is_empty() && read.next != from {
                        // The walk moved without reading a record, which only
                        // happens when the window's own first record is one the
                        // read cannot hold. The position it reached is the one
                        // the window answers, so a caller that stopped here
                        // would never see the window's rows.
                        passed = passed.saturating_add(read.skipped_bytes);
                        from = read.next;
                        continue;
                    }
                    break read;
                }
                WindowRead::Absent => {
                    let history = log_windows::fold_epoch(
                        self.backend.as_ref(),
                        session,
                        self.states.restore_budget(),
                    )?;
                    let rows = history.rows();
                    self.states.commit_restore(session, history);
                    if rows == 0 {
                        break TrajectoryPage {
                            next: from,
                            ..TrajectoryPage::default()
                        };
                    }
                }
            }
        };
        page.skipped_bytes = page.skipped_bytes.saturating_add(passed);
        if page.skipped_bytes > 0 {
            // A record wider than one log window cannot be parsed, so the
            // reader reports the span rather than spending it silently.
            tracing::warn!(
                "session {session} passed over {} bytes in records wider than a log window",
                page.skipped_bytes
            );
        }
        Ok(page)
    }

    /// The id of the latest event the log state holds, or None when it holds
    /// none yet. Reads a counter, not the window.
    pub fn last_trajectory_id(&self, session: SessionId) -> Option<EventId> {
        self.states.last_id(session)
    }

    /// The id of the latest durable user input for a session, or None when
    /// the session holds none yet. Reads a counter taken as events arrive, so
    /// an input the window has trimmed still answers.
    pub fn last_user_input_id(&self, session: SessionId) -> Option<EventId> {
        self.states.last_user_input_id(session)
    }

    /// Read the session's trajectory revision and whole-session summary under
    /// one lock, so the two describe the same revision.
    pub fn trajectory_head(&self, session: SessionId) -> TrajectoryHead {
        self.states.head(session)
    }

    /// Read a child session's full transcript from disk. Returns empty when
    /// the child log is gone (deleted or archived), so the caller falls back
    /// to the inline summary the parent already holds. Not the log state -- the
    /// parent carries the summary; this follows the ref for the full text.
    pub fn read_child_result(&self, child: SessionId) -> Vec<SessionLogEntry> {
        match self.backend.read_log(child) {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!("child session log read failed, returning empty: {e}");
                Vec::new()
            }
        }
    }

    /// Drop the in-memory log state for a session (the /clear path).
    /// The backend's append-only log is untouched — this only frees the
    /// viewable state so /trajectory reads fresh after a clear.
    pub fn reset_trajectory(&self, session: SessionId) {
        self.states.reset(session);
    }

    /// Backfill the in-memory log state from the backend log for a
    /// resumed session (read-only — no append, no chain rewrite). The state
    /// starts empty on resume; this reads the durable history so the
    /// serve-start replay ships the resumed conversation to the client (the
    /// working screen shows the past turns, not just the status bar). Returns
    /// the durable records the session's current epoch holds.
    ///
    /// A backend that reads byte windows of its own log is read one window at
    /// a time, so the process never holds the history: the fold keeps the
    /// newest rows and the counts for all of them. A backend with no byte log
    /// has nowhere else to read, so its whole replay becomes the log state.
    ///
    /// The chain's cache is not primed here. Priming from the replayed
    /// last event would re-serialize it and link the next append to a hash
    /// that drifts from the on-disk bytes under a schema change; the next
    /// append's cold read hashes the last disk line's raw bytes instead. The
    /// log state is replaced, so this must run before any
    /// append for the session: one landing mid-read is dropped by the
    /// install that follows.
    pub async fn restore_trajectory(&self, session: SessionId) -> Result<usize, ContextError> {
        let history = if self.backend.supports_log_windows() {
            let history = log_windows::fold_epoch(
                self.backend.as_ref(),
                session,
                self.states.restore_budget(),
            )?;
            if history.skipped_bytes() > 0 {
                tracing::warn!(
                    "session {session} holds {} bytes in records wider than a log window; those records are not in the log state",
                    history.skipped_bytes()
                );
            }
            history
        } else {
            let mut replayed = self.backend.replay(session).await?;
            // A cleared session's trajectory is what came after the last clear:
            // the durable log keeps the archived turns, and resuming one would
            // restore the turns and the spend the clear no longer counts.
            let boundary = epoch_start(&replayed);
            // Dropped in place: an event carries tool payloads and message
            // text, so slicing to a new vector would copy the whole suffix.
            if boundary > 0 {
                replayed.drain(..boundary);
            }
            RestoredHistory::from_events(replayed)
        };
        let count = history.rows();
        self.states.commit_restore(session, history);
        Ok(count)
    }

    /// Seed the store from an imported trajectory (an export or an old log
    /// replayed elsewhere). Two-step: (1) verify the source chain as the
    /// exporting binary wrote it -- re-serialize each event to line bytes and
    /// check prev_hash linkage, including deltas. A schema change between
    /// exporter and importer can make this mismatch, so the verdict is
    /// best-effort and recorded, not fatal. (2) Rebuild the durable subset
    /// chain by appending each durable event through the normal append path,
    /// which recomputes prev_hash over the durable subset (deltas never
    /// entered the chain, so it skips them naturally). Historical
    /// deltas are dropped entirely -- they are transport-only, never
    /// persisted and never held in the log state (their authoritative
    /// AssistantMessage is the full superset). Returns a typed report; an
    /// warning, not an error, so the rescue never hard-fails here.
    pub async fn seed_trajectory(
        &self,
        session: SessionId,
        events: Vec<SessionLogEntry>,
    ) -> Result<ImportReport, ContextError> {
        // Step 1: verify the source chain (read-only, before consuming).
        let source_chain = chain::verify_source(&events);
        // Step 2: rebuild the durable subset. Each append takes the
        // append_lock (uncontended during seed -- the runner is not driving
        // yet), recomputes prev_hash from the chain (durable-only), and
        // persists to the backend. Deltas are dropped, not appended.
        let mut durable_count = 0usize;
        let mut deltas_dropped = 0usize;
        for mut ev in events {
            if matches!(ev.event, SessionEvent::AssistantTextDelta { .. }) {
                deltas_dropped += 1;
                continue;
            }
            // Rewrite the event onto the destination session (import forks a
            // new session; the source events carry the original sid).
            ev.session = session;
            self.append(ev).await?;
            durable_count += 1;
        }
        let head_hash = self.chain.held(session);
        Ok(ImportReport {
            durable_count,
            deltas_dropped,
            source_chain,
            head_hash,
        })
    }

    /// Verify a disk log's prev_hash chain by hashing the raw line bytes it
    /// holds, from the log head. Best-effort: a parse failure or a mismatch
    /// reports Unverified with the index and reason, and the caller warns
    /// without blocking resume, because a crashed session may still recover.
    pub fn verify_disk_chain(&self, session: SessionId) -> ChainVerdict {
        chain::verify_disk(self.backend.as_ref(), session)
    }

    pub async fn replay(&self, session: SessionId) -> Result<Vec<SessionLogEntry>, ContextError> {
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

    /// Assemble the context. Reads the event log from the session's last clear
    /// boundary onward and, if a checkpoint exists, loads the latest manifest
    /// so the caller can apply the per-event Disposition plan (Verbatim /
    /// Summarized / Referenced). No checkpoint means no plan applied.
    ///
    /// The durable log keeps the turns a clear archived, because /resume reads
    /// them back; the live session's context is what followed the boundary. A
    /// manifest whose last event predates the boundary folded turns this
    /// context no longer holds, so it is dropped rather than applied.
    pub async fn current_view(&self, session: SessionId) -> Result<ContextSnapshot, ContextError> {
        let mut events = self.backend.replay(session).await?;
        let boundary = epoch_start(&events);
        if boundary > 0 {
            events.drain(..boundary);
        }
        let rewind_points = self.backend.list_checkpoints(session).await?;
        let last_checkpoint = rewind_points.last().copied();
        let manifest = match last_checkpoint {
            Some(cp) => {
                let manifest = self.backend.read_checkpoint(cp).await?;
                events
                    .iter()
                    .any(|event| event.id == manifest.last_event)
                    .then_some(manifest)
            }
            None => None,
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
impl SessionLog for SessionStore {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        Box::pin(Self::append(self, event))
    }
    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        Box::pin(Self::replay(self, session))
    }
    fn current_view(&self, session: SessionId) -> PFut<'_, Result<ContextSnapshot, ContextError>> {
        Box::pin(Self::current_view(self, session))
    }
    fn trajectory_tail(&self, session: SessionId) -> Vec<SessionLogEntry> {
        Self::trajectory_tail(self, session)
    }
    fn trajectory_page(
        &self,
        session: SessionId,
        from: TrajectoryPosition,
        limit: usize,
    ) -> Result<TrajectoryPage, ContextError> {
        Self::trajectory_page(self, session, from, limit)
    }
    fn last_trajectory_id(&self, session: SessionId) -> Option<EventId> {
        Self::last_trajectory_id(self, session)
    }
    fn last_user_input_id(&self, session: SessionId) -> Option<EventId> {
        Self::last_user_input_id(self, session)
    }
    fn trajectory_head(&self, session: SessionId) -> TrajectoryHead {
        Self::trajectory_head(self, session)
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
    fn session_log_root(&self) -> Option<PathBuf> {
        self.backend.session_log_root().map(Path::to_path_buf)
    }
}

#[cfg(test)]
mod tests;
