//! The pane's own data types: what a row is, and what the pane renders.
//!
//! Plain data, so the composition root can build it and the render path can
//! read it without either depending on the other.

use crate::state::TrajectoryTurnKey;
use houyicoder_protocol::llm::UsageProvenance;

#[derive(Clone, PartialEq, Debug)]
pub struct TrajectoryRecord {
    pub kind: TrajectoryRecordKind,
    /// The record's own name: tool name, agent type, or model id. None when the
    /// log does not name it (an unnamed context row, an old record).
    pub name: Option<String>,
    /// The record's ordinal among the calls of its kind in this turn, so a
    /// multi-call turn reads as Model 1, Tool 2, Model 3 rather than as
    /// interchangeable rows. Zero when the kind is not numbered.
    pub ordinal: u32,
    pub summary: String,
    /// Offset from the turn start, in ms. Positions the event on the shared
    /// time axis so parallel events overlap on the same columns and sequence
    /// is visible at a glance — not just duration.
    pub start_ms: u64,
    pub duration_ms: u64,
    pub outcome: RecordOutcome,
    /// Full text for the Level 2 detail view: the model's thinking, a tool's
    /// input, a tool's result or the model's reply. Held separate from summary
    /// (the one-line L1 preview) so L2 shows full content without
    /// re-truncating.
    pub thinking: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
    /// Model calls only.
    pub usage: Option<EventUsage>,
    /// Model calls only.
    pub timing: Option<EventTiming>,
    /// Model calls only: length-recovery retries folded into this call.
    pub retries: usize,
    /// The model this call switched away from, when it did. Kept as a field
    /// rather than folded into the summary so a reply that later replaces the
    /// summary cannot erase the switch.
    pub model_switch_from: Option<String>,
    /// What the block store kept of a tool result, or the read a storage row
    /// reports. None for every record that never touched the store, so the
    /// rows for ordinary work carry nothing about it.
    pub storage: Option<StorageFact>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct TrajectoryTurn {
    pub n: usize,
    /// The durable identity of the turn: the event that opened it. A turn
    /// number names a turn within one history; this names it across a clear,
    /// and it is what a detail read is asked for by.
    pub key: TrajectoryTurnKey,
    /// Boundaries the log recorded between the previous turn and this one, in
    /// the order they happened. Several durable facts can land in one gap (a
    /// compaction and then a model switch), so this is a list rather than a
    /// slot: a single slot would drop one of them without saying so. Data only;
    /// the pane decides how to draw them.
    pub boundary_before: Vec<TurnBoundary>,
    /// What the turn's row is titled: a preview of the prompt, or of the first
    /// record's summary for a turn whose prompt sits outside the loaded window.
    /// Derived where the records are, so the list needs no detail to name a row,
    /// and a preview because the row draws one line.
    pub title: String,
    pub tokens_in: Option<usize>,
    pub tokens_out: Option<usize>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    /// Which token counts this turn's calls reported, AND-folded across them
    /// and across the children folded into the same totals. A count no call
    /// reported and the log left at zero is unknown rather than a measured
    /// zero, so no share is derived over it.
    pub usage_provenance: UsageProvenance,
    /// The model of every call in the order it ran, with a repeat kept
    /// whenever another model came between: the chain a turn went through, so
    /// a switch back to an earlier model is visible.
    pub model_chain: Vec<String>,
    /// Every model id this turn called, in first-use order and de-duplicated.
    /// A turn can switch models mid-flight, so this is a list rather than a
    /// single id; empty when no TurnUsage landed or the log predates the field
    /// — unknown, not blank.
    pub models: Vec<String>,
    /// Every effort level this turn sent, in first-use order and
    /// de-duplicated. Empty when no effort parameter was sent (model
    /// unsupported, auto, or old log) — unknown, not auto.
    pub efforts: Vec<String>,
    /// Reasoning tokens across this turn's calls (a component of
    /// output_tokens, not a separate total). None when no TurnUsage landed or
    /// the log predates the field — unknown, not zero.
    pub reasoning_tokens: Option<usize>,
    /// Wall time this turn spent inside reasoning blocks, summed from the
    /// durable timing events of its calls. None when no call reported a
    /// reasoning span: a sum over the calls that reported one is a floor on
    /// what the turn thought, but a turn where nobody reported states nothing
    /// rather than zero.
    pub reasoning_ms: Option<u64>,
    pub tool_count: usize,
    pub tool_fail: usize,
    pub retries: usize,
    pub duration_ms: u64,
    pub success: bool,
}

#[derive(Clone, PartialEq, Debug)]
pub struct TrajectoryBg {
    pub kind: String,
    pub summary: String,
    pub duration_ms: u64,
}

impl TrajectoryTurn {
    /// The share of this turn's input that came from cache, when the turn's
    /// account supports one: every call reported the input total and the
    /// cache-read count, both counts are on the turn, and the input is above
    /// zero. None otherwise — a call that did not report leaves the account
    /// partial, and a share divided over a partial account can read higher than
    /// the truth, so the row claims none.
    ///
    /// A partial account still names its raw count where the log holds one: only
    /// the derived percentage is withheld.
    pub fn cache_hit_pct(&self) -> Option<f64> {
        let (Some(cache_read), Some(tokens_in)) = (self.cache_read, self.tokens_in) else {
            return None;
        };
        self.usage_provenance
            .cache_hit_pct(cache_read, tokens_in as u64)
    }

    /// The same share with the top of the range reserved for a turn that
    /// missed nothing, one decimal place wide. The list prints this one.
    pub fn cache_hit_display_pct(&self) -> Option<f64> {
        let (Some(cache_read), Some(tokens_in)) = (self.cache_read, self.tokens_in) else {
            return None;
        };
        self.usage_provenance
            .cache_hit_display_pct(cache_read, tokens_in as u64)
    }
}

/// One row of the turn list: a turn, or a background event between turns.
///
/// Nearly every row is a turn and a background event is the rare one, so the
/// union is sized for the turn: rows are held in a list and read by reference,
/// and boxing the turn would put an allocation and an indirection on every row
/// of that list to shrink the variant that is occasionally present.
#[derive(Clone)]
#[expect(
    clippy::large_enum_variant,
    reason = "the turn is nearly every row, and boxing it would allocate a heap node per row to shrink the rare one"
)]
pub enum TrajectoryRow {
    Turn(TrajectoryTurn),
    Bg(TrajectoryBg),
}

/// Session-wide latency and work-time facts, computed once from the durable
/// timing events and read by both the trajectory pane and the status pane. One
/// value, two renderers: neither surface recomputes a percentile or a rate, so
/// they cannot disagree about the session they describe.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct SessionTiming {
    /// Time-to-first-token samples and their nearest-rank percentiles. None
    /// when the session recorded no first token at all.
    pub ttft_samples: usize,
    pub ttft_avg_ms: Option<u64>,
    pub ttft_p95_ms: Option<u64>,
    pub ttft_p99_ms: Option<u64>,
    /// Decode samples: how many calls reported a decode span.
    pub decode_samples: usize,
    pub decode_tok_per_sec: Option<f64>,
    /// Wall time spent inside model calls, and inside tool executions. The two
    /// can overlap (a delegation runs while its parent waits), so they are
    /// reported separately and never added into a single total.
    pub model_ms: u64,
    pub tool_ms: u64,
}

impl SessionTiming {
    /// True when the session recorded no timing at all, so a caller hides the
    /// rows rather than printing zeroes.
    pub fn is_empty(&self) -> bool {
        self.ttft_samples == 0
            && self.decode_samples == 0
            && self.model_ms == 0
            && self.tool_ms == 0
    }
}

/// What delegated sub-agents spent, summed over the session's delegations.
///
/// The session totals already include it: both the trajectory totals and the
/// status tally read the durable SubagentReturn boundaries. This type is the
/// breakdown, so a surface can say how much of the total the children
/// contributed instead of only reporting one undivided number.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct SubagentUsage {
    pub calls: usize,
    /// Children that returned without reporting any usage. Their spend is
    /// missing from the figures below, so a row that names them states a floor
    /// instead of presenting the sum as the whole.
    pub unmeasured: usize,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// Which breakdown fields the children reported, AND-folded. A child that
    /// did not report its cache-read leaves the count at zero, so without this
    /// the zero reads as a cache miss.
    pub provenance: UsageProvenance,
}

impl SubagentUsage {
    /// The share of the delegated input that came from cache, from the same
    /// derivation the per-turn shares use: None unless the children reported
    /// both the input total and the cache-read count, so an unreported count
    /// reads as unknown rather than as a miss.
    pub fn cache_hit_pct(&self) -> Option<f64> {
        self.provenance.cache_hit_pct(self.cache_read, self.input)
    }
}

/// What the session's recaps spent, summed over the ones it wrote.
///
/// The session totals already include it: a recap runs between turns and its
/// cost belongs to the session, not to any turn. This type is the breakdown,
/// so a reader can say how much of the total the recaps contributed instead
/// of leaving it indistinguishable from the work the user asked for.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct RecapSpend {
    pub calls: usize,
    /// Recaps that wrote no token figure. Their spend is missing from the
    /// figures below, so a row that names them states a floor instead of
    /// presenting the sum as the whole.
    pub unmeasured: usize,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// Wall time the recaps measured; None when none of them measured any.
    pub duration_ms: Option<u64>,
    /// Which breakdown fields the recaps reported, AND-folded. A recap that
    /// did not report its cache-read leaves the count at zero, so without this
    /// the zero reads as a cache miss.
    pub provenance: UsageProvenance,
}

/// What the pane can say about its data right now.
///
/// The rows come from a bounded page read, so the first frame after a session
/// is opened has nothing to show yet. That is a state of its own: a pane that
/// rendered an empty list, or fell back to the demonstration rows, would tell
/// the user the session has no turns when the read simply has not landed.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum TrajectoryViewState {
    /// The first page is being read.
    Loading,
    /// A page is loaded and the rows below it are real.
    #[default]
    Ready,
    /// Older turns are being read; the rows already shown stay.
    LoadingOlder,
    /// The read failed, so there is nothing truthful to show.
    Failed,
}

#[derive(Clone, Default)]
pub struct TrajectoryView {
    pub session_id: String,
    /// Derived: one model when every turn's model field matches (or is
    /// None); "N models" when ≥2 distinct ids appear. Replaces the
    /// construction-time string snapshot so a mid-session model switch
    /// surfaces immediately.
    pub model: String,
    /// Distinct model ids the session used, so a row can decide whether to
    /// name its own model from a session fact rather than from the window.
    pub models_used: usize,
    pub total_turns: usize,
    pub tokens_in: Option<usize>,
    pub tokens_out: Option<usize>,
    /// The cache-read count the session's log holds, or None when the count
    /// is unknown and nothing was reported cached. A count the endpoints
    /// reported, or one above zero, is carried as it stands: it is a floor on
    /// what the session reused.
    pub cache_read: Option<u64>,
    /// Which of the session's counts the endpoints reported, folded by the one
    /// fold that owns the session's account. A share is derived from this
    /// rather than from the counts alone: the totals include the delegated
    /// children, so a child that reported nothing leaves the account partial
    /// even when every one of the session's own turns reported.
    pub usage_provenance: UsageProvenance,
    pub failures: usize,
    /// Tool calls the session issued, from the session summary rather than
    /// from the rows the window happens to hold.
    pub tool_calls: usize,
    /// The session's wall time: the span between its first and last durable
    /// event, or None when the log carried no event to measure between.
    pub duration_ms: Option<u64>,
    pub timing: SessionTiming,
    /// How many turns sit before the loaded window. Non-zero means older
    /// history exists and has not been read yet.
    pub hidden_turns: usize,
    /// How many turns sit after the loaded window. Non-zero means the window
    /// has been walked back from the tail and newer turns are not loaded; End
    /// returns to them.
    pub newer_hidden: usize,
    /// Which history these rows belong to. It changes when a clear starts a
    /// new one, and the turn numbers of a new history name different turns,
    /// so a selection made under the old one must not be restored.
    pub history_generation: u64,
    /// What delegated sub-agents spent, when the session delegated any work.
    pub subagent_usage: Option<SubagentUsage>,
    /// What the session's away and resume recaps spent, when it wrote any.
    pub recap_usage: Option<RecapSpend>,
    /// Whether the rows below are loaded, still loading, or unavailable.
    pub state: TrajectoryViewState,
    /// Log lines in the loaded window that could not be read. The rest of the
    /// window is still shown; a surface that stayed silent about them would
    /// report a session as smaller than it is.
    pub skipped_records: usize,
    pub rows: Vec<TrajectoryRow>,
}

/// What one timeline record represents. These are domain names: a record is a
/// model call, a tool call, a piece of context, or an agent, not a raw log
/// variant and not a provider stream detail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrajectoryRecordKind {
    /// User input, a mid-turn update, or context injected by the runner.
    Context,
    /// One model call: thinking, response, usage, and timing.
    Model,
    /// One tool call together with its result.
    Tool,
    /// A delegated sub-agent, spanning spawn to return.
    Agent,
    /// Memory recalled into this turn.
    Memory,
    /// A prompt-cache boundary: the provider reused less of the prefix than
    /// the prior call, with the cause the agent loop attributed it to.
    Cache,
    /// A hook verdict that did not deny: an observation, an injection, or a
    /// request for the user. Not a failure, so it is not drawn as one.
    Hook,
    /// Context folded away by compaction.
    Compaction,
    /// A turn-level failure or interruption marker.
    Error,
    /// What the block store did in this turn: a read of a stored result, or a
    /// turn that never kept one.
    Storage,
}

impl TrajectoryRecordKind {
    /// The kind column label. Fixed width so the columns to its right line up.
    pub fn label(self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::Model => "model",
            Self::Tool => "tool",
            Self::Agent => "agent",
            Self::Memory => "memory",
            Self::Cache => "cache",
            Self::Hook => "hook",
            Self::Compaction => "compact",
            Self::Error => "error",
            Self::Storage => "storage",
        }
    }
}

/// Token facts for one model call. Every field is optional: a provider that
/// omits usage leaves the value unknown, never zero.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct EventUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
    /// Which counts the call's endpoint reported, AND-folded across the retries
    /// folded into this call. A count it did not report and the log left at
    /// zero is unknown rather than a measured zero, so the drill consults this
    /// before printing a count.
    pub provenance: UsageProvenance,
}

/// Latency facts for one model call, as measured by the agent loop. A call
/// that produced no first token (aborted or failed) leaves the split unknown
/// while still carrying its total wall time.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct EventTiming {
    pub total_ms: u64,
    pub ttft_ms: Option<u64>,
    pub decode_ms: Option<u64>,
    /// Time the call spent inside its reasoning blocks, when the stream closed
    /// any. None is unknown: the provider sent no reasoning, or the call ended
    /// inside one, and neither is a measurement of zero.
    pub reasoning_ms: Option<u64>,
    /// Time the call spent inside its output blocks, the reply text and a tool
    /// call's arguments alike, summed the same way.
    pub response_ms: Option<u64>,
}

/// How a record ended. Three states, not a boolean: a tool call whose result
/// has not landed yet is not a success, and claiming it succeeded would put a
/// checkmark on work that may still fail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordOutcome {
    Ok,
    Failed,
    /// Started but not finished inside the loaded window.
    Pending,
}

impl RecordOutcome {
    /// The status glyph the timeline and the detail header show.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Ok => "✓",
            Self::Failed => "✗",
            Self::Pending => "…",
        }
    }
}

/// A durable boundary recorded between two turns.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TurnBoundary {
    /// The user cleared the conversation context before this turn. prior_turn
    /// is the model-call count at the clear; at_secs is the durable event's
    /// timestamp, so the pane can say when it happened.
    ContextCleared { prior_turn: u32, at_secs: u64 },
    /// The model changed between this turn and the previous one.
    ModelSwitch(Box<ModelSwitchBoundary>),
    /// A compaction occurred before this turn.
    Compacted(Box<CompactedBoundary>),
}

/// Facts for a model-switch boundary between turns.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModelSwitchBoundary {
    pub from: String,
    pub to: String,
    pub at_secs: u64,
}

/// Facts for a compaction boundary between turns. The token counts bracket
/// the fold, so the separator shows what it reclaimed. Both are zero on a log
/// written before the counts were recorded.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CompactedBoundary {
    pub checkpoint_id: String,
    pub pre_tokens: u64,
    pub post_tokens: u64,
    pub at_secs: u64,
}

/// A stored block a tool result was replaced by: its address, shortened where
/// a row has no room for the full one, and the bytes it holds.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StoredBlock {
    pub hash: String,
    pub bytes: u64,
}

impl StoredBlock {
    /// The first eight characters of the address, which is what a row prints.
    pub fn short(&self) -> &str {
        short_address(&self.hash)
    }
}

/// The prefix of a block address a row prints. One rule for every view that
/// names a block: an address is compared by its prefix and never shown whole
/// in a row. Block addresses are hex hashes, so the first eight bytes are
/// always ASCII and the byte slice is a char boundary.
pub fn short_address(address: &str) -> &str {
    &address[..address.len().min(8)]
}

/// The form an assembly took for a stored block. The names match the account
/// the context view prints, so one block is described one way across views.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockForm {
    /// The full stored content, retrieved from the store.
    Materialize,
    /// The inline preview the tool result carried, with no retrieval.
    Summarize,
    /// The pointer alone, with no retrieval.
    Evict,
    /// The stored content could not be read.
    Unavailable,
    /// A form written by a build this one does not know.
    Unknown,
}

impl BlockForm {
    pub fn label(self) -> &'static str {
        match self {
            Self::Materialize => "full",
            Self::Summarize => "preview",
            Self::Evict => "pointer",
            Self::Unavailable => "unavailable",
            Self::Unknown => "unknown",
        }
    }
}

/// Who read a stored result. A reader this build does not know reads back as
/// unknown rather than being counted as either.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadActor {
    Model,
    Ui,
    Unknown,
}

impl ReadActor {
    pub fn label(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Ui => "UI",
            Self::Unknown => "unknown",
        }
    }
}

/// Whether a read delivered the bytes it asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadOutcome {
    Read,
    Failed,
    Unknown,
}

impl ReadOutcome {
    /// The row glyph, on the same three-way scale every other record uses: a
    /// read that delivered nothing is not a success.
    pub fn outcome(self) -> RecordOutcome {
        match self {
            Self::Read => RecordOutcome::Ok,
            Self::Failed => RecordOutcome::Failed,
            Self::Unknown => RecordOutcome::Pending,
        }
    }
}

/// One read of a stored result, as the log recorded it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StorageRead {
    /// The result the read addressed, as the host's catalog names it.
    pub result_id: String,
    /// The block holding that result's bytes, when the log names it.
    pub block: Option<String>,
    pub actor: ReadActor,
    pub outcome: ReadOutcome,
    /// First byte of the range the read covered; zero when it delivered
    /// nothing.
    pub start: u64,
    /// Bytes the read returned.
    pub bytes: u64,
}

/// What the block store kept of one tool result, and what the turn's request
/// and reads said about it.
///
/// The four stages are separate facts, because a command can print its full
/// output and still lose it: captured states what the host kept raw, the block
/// states what the model view was given in place of it, the form states how
/// that block was carried, and the reads state who later opened it. A reader
/// that merges them cannot say which stage lost the output.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct StorageFact {
    /// Raw bytes the capture kept, when the result names one.
    pub raw_bytes: Option<u64>,
    /// What stopped the command before its output ended.
    pub stopped: Option<String>,
    /// Whether the capture refused to publish the raw output.
    pub save_failed: bool,
    /// The block the model view carried in place of the output, when the
    /// result names one with its stored size.
    pub block: Option<StoredBlock>,
    /// The form the newest assembly took for that block, which assembly
    /// carried it, and when that request was assembled.
    pub form: Option<BlockForm>,
    pub assembly_id: Option<u64>,
    pub assembly_at_ms: Option<u64>,
    /// Reads of this result the log recorded in the turn that was folded.
    /// Empty means this fold saw none, which is not the same as a measured
    /// zero: a read recorded in a turn outside the loaded window is invisible
    /// here.
    pub reads: Vec<StorageRead>,
}

impl StorageFact {
    /// True when there is anything to say about the store, so an ordinary
    /// tool result carries no storage facts at all.
    pub fn is_empty(&self) -> bool {
        self.raw_bytes.is_none()
            && self.stopped.is_none()
            && !self.save_failed
            && self.block.is_none()
            && self.form.is_none()
            && self.assembly_id.is_none()
            && self.assembly_at_ms.is_none()
            && self.reads.is_empty()
    }

    /// True when the fact states something about what the capture kept, which
    /// is the record a four-stage account describes. A read row carries only
    /// its own read, so it states none of it.
    pub fn has_capture_evidence(&self) -> bool {
        self.raw_bytes.is_some()
            || self.stopped.is_some()
            || self.save_failed
            || self.block.is_some()
    }
}
