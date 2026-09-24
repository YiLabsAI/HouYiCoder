//! The append-only session log port: the engine-facing contract for event
//! logging, replay, and cursor (checkpoint) support. Signatures reference
//! context types (SessionLogEntry, ContextSnapshot). The concrete facade (hash
//! chain, delta counter, trajectory mirror) lives in the session crate; the
//! engine depends on this trait so it does not depend on the session crate
//! directly.

use std::sync::Arc;

use houyicoder_async::PFut;
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, ContextSnapshot, EventId,
    SessionEvent, SessionId, SessionLogEntry,
};
use houyicoder_protocol::llm::Usage;

/// The delegated usage a session's children reported in durable returns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubagentUsage {
    /// Children that reached a terminal, whether or not they reported usage.
    pub calls: usize,
    /// Children that reached a terminal without reporting any usage.
    pub unmeasured_calls: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub reasoning_tokens: u64,
}

impl SubagentUsage {
    /// Fold one durable event into the projection. Events other than a child
    /// return leave it unchanged.
    pub fn record(&mut self, event: &SessionEvent) {
        let SessionEvent::SubagentReturn {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_tokens,
            ..
        } = event
        else {
            return;
        };
        self.calls = self.calls.saturating_add(1);
        if *input_tokens == 0
            && *output_tokens == 0
            && *cache_read_input_tokens == 0
            && *cache_write_input_tokens == 0
            && *reasoning_tokens == 0
        {
            self.unmeasured_calls = self.unmeasured_calls.saturating_add(1);
        }
        self.input_tokens = self.input_tokens.saturating_add(*input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(*output_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(*cache_read_input_tokens);
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .saturating_add(*cache_write_input_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(*reasoning_tokens);
    }

    /// Convert the projection to the provider usage shape used by status.
    pub fn to_usage(self) -> Usage {
        let clamp = |v: u64| v.min(u32::MAX as u64) as u32;
        let input_tokens = clamp(self.input_tokens);
        let output_tokens = clamp(self.output_tokens);
        Usage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens.saturating_add(output_tokens),
            non_cached_input_tokens: clamp(
                self.input_tokens
                    .saturating_sub(self.cache_read_input_tokens),
            ),
            cache_read_input_tokens: clamp(self.cache_read_input_tokens),
            cache_write_input_tokens: clamp(self.cache_write_input_tokens),
            reasoning_tokens: clamp(self.reasoning_tokens),
        }
    }
}

/// Sum the delegated usage in a durable event slice.
pub fn aggregate_subagent_usage(events: &[SessionLogEntry]) -> SubagentUsage {
    let mut total = SubagentUsage::default();
    for event in events {
        total.record(&event.event);
    }
    total
}

/// How far the session's mirror has advanced. Ids are monotonic within a
/// process, so a caller that kept the last count can ask for exactly what was
/// appended since; the id is a cross-check that the mirror was not replaced
/// under a smaller count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrajectoryRevision {
    /// Events in the mirror, streaming deltas included, because a caller
    /// resumes its own cursor in that same index space.
    pub event_count: usize,
    /// The newest event's id, or None for an empty mirror.
    pub last_event_id: Option<EventId>,
    /// Durable events in the mirror, streaming deltas excluded.
    ///
    /// A delta arrives many times per second while a model streams, so a
    /// caller that re-reads the log whenever the revision moves has to key on
    /// this instead: the durable history is what a disk read would return, and
    /// a delta does not change it.
    pub durable_event_count: usize,
    /// The newest durable event's id, or None when none has been appended.
    pub last_durable_event_id: Option<EventId>,
    /// The durable event that began the current epoch: the first event of the
    /// mirror, which after a clear is the clear itself.
    ///
    /// A reader holding a page needs this to tell two cases apart. An append
    /// moves the count but leaves the epoch alone, so a page read before it is
    /// still the same session and may be shown. A clear starts a new epoch, so
    /// a page read before it describes turns the session no longer counts and
    /// must never be shown.
    pub epoch_event_id: Option<EventId>,
}

/// The session's token account.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrajectoryUsageSummary {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    /// False when any turn reached its end without reporting usage, so the
    /// token figures are a lower bound rather than the session's cost.
    pub totals_known: bool,
    pub failures: usize,
    /// Tool calls the session issued, counted from the calls themselves so a
    /// header can report the session rather than the page it has loaded.
    pub tool_calls: usize,
    /// What the session's delegated children spent.
    pub subagent: SubagentUsage,
    /// True when a child reached a terminal without reporting usage.
    pub subagent_unmeasured: bool,
}

/// The session's latency facts.
///
/// The average is exact. The percentiles are bucket upper bounds: the samples
/// are counted into a fixed set of buckets so recording stays constant time
/// and a read stays bounded, which means a percentile is reported to the width
/// of its bucket rather than to the millisecond. A renderer must not print one
/// as if it were exact.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TrajectoryTimingSummary {
    pub ttft_samples: usize,
    pub ttft_avg_ms: Option<u64>,
    pub ttft_p95_ms: Option<u64>,
    pub ttft_p99_ms: Option<u64>,
    /// True when a percentile fell in the overflow bucket, so the reported
    /// value is a floor rather than a bucket bound.
    pub ttft_percentile_capped: bool,
    pub decode_samples: usize,
    pub decode_tok_per_sec: Option<f64>,
    pub model_ms: u64,
    pub tool_ms: u64,
}

/// The whole-session figures the trajectory header and the status pane report.
///
/// These cannot come from a loaded window: a session's turn count, its token
/// account, and its latency distribution are facts about every turn it ran,
/// including the ones a page has not read. The store folds them as it appends,
/// so a read copies small numbers instead of scanning the log.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrajectorySummary {
    /// Turns as the pane numbers them, which counts a log that opens mid-run
    /// as carrying one more turn than it has user inputs.
    pub total_turns: usize,
    pub usage: TrajectoryUsageSummary,
    pub timing: TrajectoryTimingSummary,
    /// Distinct model ids across the session, not across the loaded page.
    pub models_used: usize,
    /// The one model the session used, when exactly one appears. The append
    /// path interns it, so a read only clones an Arc and allocates nothing:
    /// this is read once a second by status and once a frame by the pane.
    pub single_model: Option<Arc<str>>,
    /// The span of the session's own events, which is what the user waited.
    pub duration_ms: u64,
}

/// One consistent read of a session's trajectory state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrajectoryHead {
    pub revision: TrajectoryRevision,
    pub summary: TrajectorySummary,
}

/// The engine-facing session log. Object-safe (PFut) so the engine holds
/// Arc<dyn SessionLog> and the concrete session facade swaps behind it. The
/// facade layers the hash chain, delta-persistence counter, and trajectory
/// mirror on top of a ContextBackend; this port surfaces only what the engine
/// drives.
pub trait SessionLog: Send + Sync {
    /// Append an event to the lossless log. The facade sets prev_hash; the
    /// backend stores it verbatim.
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>>;

    /// Read the full event log for a session in append order.
    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>>;

    /// Assemble the context: the full replay plus the latest
    /// checkpoint manifest, so the caller can apply the disposition plan.
    fn current_view(&self, session: SessionId) -> PFut<'_, Result<ContextSnapshot, ContextError>>;

    /// The finalized events in append order (sync, in-memory mirror). Empty
    /// until events are appended this process for the session.
    fn trajectory_snapshot(&self, session: SessionId) -> Vec<SessionLogEntry>;

    /// The id of the latest mirrored event, or None when the session has no
    /// mirror yet. Defaults to reading the snapshot; a store with an indexed
    /// mirror overrides it to answer without cloning the log.
    fn last_trajectory_id(&self, session: SessionId) -> Option<EventId> {
        self.trajectory_snapshot(session).pop().map(|e| e.id)
    }

    /// Clone the finalized suffix beginning at start from the in-memory mirror.
    /// Implementations should avoid cloning the already-consumed prefix.
    fn trajectory_since(&self, session: SessionId, start: usize) -> Vec<SessionLogEntry> {
        self.trajectory_snapshot(session)
            .into_iter()
            .skip(start)
            .collect()
    }

    /// The session's trajectory revision and its whole-session summary, read
    /// under one lock so the two describe the same moment.
    ///
    /// Required rather than defaulted: a status poll and every pane draw read
    /// this, so an implementation that silently inherited a scan of the log
    /// would turn a constant-time read into linear work.
    fn trajectory_head(&self, session: SessionId) -> TrajectoryHead;

    /// Drop the in-memory trajectory mirror for a session. The backend log
    /// is untouched.
    fn reset_trajectory(&self, session: SessionId);

    /// Persist a compaction manifest (checkpoint).
    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>>;

    /// Read a checkpoint manifest by id.
    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>>;

    /// List checkpoint ids for a session, oldest first.
    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>>;

    /// Borrow the underlying backend for CAS operations (block_put /
    /// block_get) from the projection layer without owning the store.
    fn backend(&self) -> &dyn ContextBackend;

    /// The durable sessions root this log's backend persists under, for
    /// cross-session readers (the dream's retry scan). Derived from the
    /// backend, not configured, so a reader can never disagree with the
    /// writer about the root. None on a backend that is not disk-backed -
    /// an in-memory build carries no cross-session history and must not
    /// read the real home.
    fn session_log_root(&self) -> Option<std::path::PathBuf> {
        None
    }
}
