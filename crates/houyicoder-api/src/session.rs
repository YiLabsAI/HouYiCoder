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
use houyicoder_protocol::llm::{RecordedCounts, Usage, UsageProvenance};
use std::path::PathBuf;

/// The delegated usage a session's children reported in durable returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// The AND-fold of every child's reported-field provenance. Seeded to
    /// all-reported (the merge identity) so a childless record stays complete
    /// and the first child that did not report a field dilutes it to unknown.
    pub provenance: UsageProvenance,
}

impl Default for SubagentUsage {
    fn default() -> Self {
        Self {
            calls: 0,
            unmeasured_calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            provenance: UsageProvenance::ALL_REPORTED,
        }
    }
}

impl SubagentUsage {
    /// Fold one durable event into the totals. Events other than a child
    /// return leave it unchanged.
    pub fn record(&mut self, event: &SessionEvent) {
        let SessionEvent::SubagentReturn {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_tokens,
            input_reported,
            cache_read_reported,
            cache_write_reported,
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
        self.provenance = self.provenance.merge(UsageProvenance {
            input_reported: *input_reported,
            cache_read_reported: *cache_read_reported,
            cache_write_reported: *cache_write_reported,
        });
    }

    /// Convert the totals to the provider usage shape used by status. The
    /// provenance is the AND-fold recorded from the children: a childless
    /// record keeps the all-reported identity, so folding it into a session's
    /// own usage leaves that usage's completeness untouched, while a record
    /// holding a child that did not report a field reads unknown rather than
    /// a silent guess.
    pub fn to_usage(self) -> Usage {
        Usage::from_recorded(RecordedCounts {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
            cache_write_input_tokens: self.cache_write_input_tokens,
            reasoning_tokens: self.reasoning_tokens,
            provenance: self.provenance,
        })
    }
}

/// The usage one recorded turn accounts for, together with the model that ran
/// it. None for an event that records no usage.
///
/// A record keeps the counts and whether the endpoint reported each of them,
/// and no total. Normalizing it here is the one path from a logged turn to the
/// usage shape the views read, so the tallies a session's spend is read from
/// cannot disagree about what a turn cost.
pub fn recorded_turn_usage(event: &SessionEvent) -> Option<(&str, Usage)> {
    let SessionEvent::TurnUsage {
        model,
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_write_input_tokens,
        reasoning_tokens,
        input_reported,
        cache_read_reported,
        cache_write_reported,
        ..
    } = event
    else {
        return None;
    };
    let counts = RecordedCounts {
        input_tokens: *input_tokens,
        output_tokens: *output_tokens,
        cache_read_input_tokens: *cache_read_input_tokens,
        cache_write_input_tokens: *cache_write_input_tokens,
        reasoning_tokens: *reasoning_tokens,
        provenance: UsageProvenance {
            input_reported: *input_reported,
            cache_read_reported: *cache_read_reported,
            cache_write_reported: *cache_write_reported,
        },
    };
    Some((model.as_str(), Usage::from_recorded(counts)))
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

/// The session's token account: what the parent's own calls and every
/// delegated child spent together, which is the figure the headline reports.
/// The delegated half is also carried on its own, so a reader that wants the
/// child contribution reads it there rather than subtracting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrajectoryUsageSummary {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    /// False when any turn reached its end without reporting usage, so the
    /// token figures are a lower bound rather than the session's cost.
    pub totals_known: bool,
    /// Which of the counts above the endpoints reported, folded across every
    /// turn and every child by the one fold that owns the session's account.
    /// A reader asks this whether a share is claimable rather than combining
    /// the parent's and the delegated half itself: a parent that reported
    /// everything and a child that reported nothing is not a complete
    /// account, and a reader left to work that out would eventually read it
    /// as one.
    pub provenance: UsageProvenance,
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

/// Where a paged trajectory read resumes.
///
/// The index counts durable records on the session's current epoch: 0 is the
/// record the newest clear boundary begins at. It is the record's ordinal in
/// the durable log, which is what makes it a position a reader can name once
/// and return to after a restart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrajectoryPosition {
    /// Absolute index of the next record to read.
    pub index: usize,
    /// The byte position the index begins at in the durable log, when this
    /// position came from a log read. None while the resident window answers.
    pub byte_offset: Option<u64>,
}

impl TrajectoryPosition {
    /// The position a read of the whole history starts at.
    pub fn start() -> Self {
        Self::default()
    }
}

/// The records one page read asks for. Each page is dropped once the reader
/// has seen it, so this is the memory a walk of the whole history costs.
const PAGE_EVENTS: usize = 512;

/// One bounded page of a session's durable history.
#[derive(Debug, Default, Clone)]
pub struct TrajectoryPage {
    /// Durable records in append order, at most the limit the reader asked
    /// for. Streamed text deltas are absent: the log carries none, so a read
    /// that serves part of its answer from the log cannot reproduce them.
    pub events: Vec<SessionLogEntry>,
    /// Where the next page continues.
    pub next: TrajectoryPosition,
    /// Bytes this page passed over because no complete record fit the log's
    /// read window. A record wider than that window cannot be parsed, so the
    /// reader reports the span instead of presenting a gap as a neighbour.
    /// Zero when the page read every record it covered.
    pub skipped_bytes: u64,
}

/// A session's durable history as successive bounded pages, read from an
/// absolute position to the end of what the store can serve.
///
/// A reader that must see every record reads these pages rather than the
/// resident tail: the tail holds only the newest rows, so a reader that took
/// it for the history would answer with the part memory happens to hold and
/// would report a session whose front was dropped as a whole one. Each page is
/// released as the next is read, so reading a history this way costs one page
/// of memory however long the history is.
///
/// The read ends at the first page without records, which is how a store
/// reports a history with nothing left to serve.
pub struct TrajectoryPages<'a> {
    log: &'a dyn SessionLog,
    session: SessionId,
    position: TrajectoryPosition,
    done: bool,
}

impl<'a> TrajectoryPages<'a> {
    /// The pages of a session's durable history, from a position to its end.
    pub fn new(log: &'a dyn SessionLog, session: SessionId, from: TrajectoryPosition) -> Self {
        Self {
            log,
            session,
            position: from,
            done: false,
        }
    }
}

impl Iterator for TrajectoryPages<'_> {
    type Item = Result<TrajectoryPage, ContextError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let page = match self
            .log
            .trajectory_page(self.session, self.position, PAGE_EVENTS)
        {
            Ok(page) => page,
            Err(e) => {
                // A failed read ends the walk: retrying the same position
                // would report the same failure until something else changes.
                self.done = true;
                return Some(Err(e));
            }
        };
        if page.events.is_empty() {
            self.done = true;
            return None;
        }
        // A page that did not move the position describes records the store
        // cannot serve past, so the read stops after reporting it once.
        let moved = page.next != self.position;
        self.position = page.next;
        if !moved {
            self.done = true;
        }
        Some(Ok(page))
    }
}

/// The id of the latest durable user input in a finalized event slice, or
/// None when the slice holds none. Only a fresh prompt counts: a mid-turn
/// input continues the turn it arrived in, so it does not move the frontier.
pub fn last_user_input_id(events: &[SessionLogEntry]) -> Option<EventId> {
    events
        .iter()
        .rev()
        .find(|entry| matches!(entry.event, SessionEvent::UserInput { .. }))
        .map(|entry| entry.id)
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

    /// Assemble the context: the log from the session's last clear boundary
    /// onward, plus the latest checkpoint manifest that describes it, so the
    /// caller can apply the disposition plan. The turns a clear archived stay
    /// readable through replay.
    fn current_view(&self, session: SessionId) -> PFut<'_, Result<ContextSnapshot, ContextError>>;

    /// The newest mirrored rows in append order (sync, in-memory mirror),
    /// streamed text deltas included. Empty until events are appended this
    /// process or the mirror is restored.
    ///
    /// This is a tail, not a promise about the whole history: a store whose
    /// backend keeps a byte-addressable log holds only its newest rows in
    /// memory, and a reader that must see every record pages with
    /// trajectory_page instead. A store whose backend keeps no log holds the
    /// whole history here, because there is nowhere else to read it.
    fn trajectory_tail(&self, session: SessionId) -> Vec<SessionLogEntry>;

    /// Read one bounded page of the session's durable history from an absolute
    /// position, so a reader can walk a history larger than memory.
    ///
    /// The resident window answers a position it covers; below that the
    /// backend's log does. A below-window position without a byte offset is
    /// resolved by reading from where the epoch begins, which costs one parse
    /// per record skipped -- prefer carrying the offset a page returned.
    ///
    /// A position at or past the newest record reads as an empty page whose
    /// next position is the same, which is how a walk ends.
    fn trajectory_page(
        &self,
        session: SessionId,
        from: TrajectoryPosition,
        limit: usize,
    ) -> Result<TrajectoryPage, ContextError>;

    /// The id of the latest mirrored event, or None when the session has no
    /// mirror yet. Defaults to reading the tail; a store that tracks the
    /// latest id as it appends overrides it to answer without cloning rows.
    fn last_trajectory_id(&self, session: SessionId) -> Option<EventId> {
        self.trajectory_tail(session).pop().map(|e| e.id)
    }

    /// The id of the latest durable user input for a session, or None when
    /// the session holds none yet. A background pass compares this against its
    /// own trigger to tell a change of the turn that just completed from one
    /// the user has already moved past. Defaults to scanning the tail; a store
    /// whose mirror is a tail overrides it, because the input it looks for may
    /// have been trimmed out of the window.
    fn last_user_input_id(&self, session: SessionId) -> Option<EventId> {
        last_user_input_id(&self.trajectory_tail(session))
    }

    /// What the session's delegated children spent, read from the summary the
    /// store folds as it appends. A store that maintains a summary answers in
    /// constant time; the default reads it from the head.
    fn subagent_usage(&self, session: SessionId) -> SubagentUsage {
        self.trajectory_head(session).summary.usage.subagent
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
    /// block_get) through the log without owning the store.
    fn backend(&self) -> &dyn ContextBackend;

    /// The durable sessions root this log's backend persists under, for
    /// cross-session readers (the dream's retry scan). Derived from the
    /// backend, not configured, so a reader can never disagree with the
    /// writer about the root. None on a backend that is not disk-backed -
    /// an in-memory build carries no cross-session history and must not
    /// read the real home.
    fn session_log_root(&self) -> Option<PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests;
