//! The append-only log's durable types: one logged record (SessionLogEntry)
//! and the vocabulary of what a record can be (SessionEvent). Every variant
//! is serialized into a session log that a later build has to read back, so
//! these types carry the log's compatibility contract and the module owns its
//! record families directly rather than leaving them in the crate root.

use serde::{Deserialize, Serialize};

use crate::hook_types::{HookErrorKind, HookEventKind, HookVerdictKind};
use crate::ids::AgentId;
use crate::ids::{CheckpointId, EventId, PrevHash, SessionId};

/// The block-store observability records the log carries.
mod cas;
pub use cas::{CachedModelView, CachedRead, ReadActor, ReadOutcome, ViewForm};

/// One record in the append-only session log. The wire type stored by a
/// ContextBackend. Replay walks entries in append order; the tool_use/
/// tool_result pair invariant (a ToolResult links back to its ToolCall via
/// call_id) must survive every cut — a replay or view that orphans one half
/// is a bug.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionLogEntry {
    pub id: EventId,
    pub session: SessionId,
    pub ts: u64,
    pub prev_hash: Option<PrevHash>,
    /// The session event this record carries.
    pub event: SessionEvent,
}

/// The classifications a logged record states about an outcome.
mod verdicts;
pub use verdicts::{PermissionVerdict, RunCompletionStatus, TruncationSignal};

/// One block_ref's frozen retention decision as the session log records it.
/// The decision is a label rather than the policy enum so this crate stays a
/// leaf; a reader skips a label it does not know instead of guessing a tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedBlockDecision {
    pub block_ref: String,
    pub decision: String,
}

/// What started a recap attempt. The two entries differ in what cancels them:
/// the window gaining focus back cancels the away entry, while the resume
/// entry is entered by that same focus, so it runs through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecapSource {
    Away,
    Resume,
    /// A source this build does not know (written by a newer version), so a
    /// newer log still reads instead of failing the line.
    #[serde(other)]
    Unknown,
}

/// What one recap request cost, as its endpoint reported it. Every figure is
/// optional because an endpoint may omit usage; an absent one is unknown, not
/// a zero-cost call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecapUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<u64>,
    /// Wall time of the whole request, unknown when the host recorded none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// The reasons a notification was injected into a session's context.
mod notification;
pub use notification::NotificationTopic;

/// Default for a measured ledger: a return written before the flag existed
/// carried the figures its writer measured, so an absent flag reads as
/// measured rather than as unknown.
fn measured_by_default() -> bool {
    true
}

/// The payload of a SessionLogEntry. Tagged for JSONL. ToolResult carries call_id
/// linking to the ToolCall it answers — the pair invariant. PermissionDecision
/// carries the call_id of the tool call the verdict answers, closing the audit
/// gap where a verdict lived only in the transient transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionEvent {
    UserInput {
        text: String,
    },
    /// A logical turn boundary. Appended at the model-call entry (the same
    /// seam as the in-memory start_turn) so every turn — including cancelled
    /// / errored ones that never reach a TurnUsage — carries a durable
    /// boundary marker. Readers group turns on this, NOT on UserInput (a
    /// single prompt spans N turns of tool iterations; grouping on UserInput
    /// flattens them and hides the retry/iteration work). turn is the logical
    /// turn number; call_in_turn is 0 at the boundary (the per-round-trip
    /// index lives on TurnUsage). Old logs have no such event — a reader then
    /// falls back to UserInput grouping.
    #[serde(rename = "TurnStarted")]
    TurnStarted {
        #[serde(default)]
        turn: u32,
        #[serde(default)]
        call_in_turn: u32,
    },
    /// A user-role message the runner injects for control purposes (e.g. the
    /// "resume directly" nudge after a token-cap truncation), not authored by
    /// the human. Reaches the model like UserInput, but hosts skip it when
    /// folding to the readable transcript so the user never sees the nudge.
    #[serde(rename = "MetaUser")]
    MetaUser {
        text: String,
    },
    /// A user message the human queued while a run was in flight, drained +
    /// appended at the next turn boundary.
    /// The durable text is the user's bare input (the transcript shows it
    /// verbatim, like UserInput). The model input assembly wraps it with a
    /// framing note so the model reads it as a mid-work interjection
    /// ("continue your current task and address it"), not a fresh
    /// instruction that drops the in-flight task.
    #[serde(rename = "MidTurnInput")]
    MidTurnInput {
        text: String,
        /// Frontend queue identity when this input originated there. Absent
        /// for legacy logs and internal steering messages.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_input_id: Option<u64>,
    },
    /// A per-turn recall of memory entries the runner injects as a user-role
    /// system-reminder attachment at the conversation tail, not authored by
    /// the human. The keys field carries the surfaced memory file stems so the
    /// next turn recall can dedup-scan the transcript. The checkpoint
    /// planner disposes this as Summarized so compaction folds it out of the
    /// assembled context, which makes surfaced scanning naturally reset with
    /// no explicit clear.
    #[serde(rename = "MemoryRecall")]
    MemoryRecall {
        text: String,
        #[serde(default)]
        keys: Vec<String>,
        /// The recall payload size in bytes (text.len()), recorded inline so
        /// the self-evolution loop can query per-turn recall cost from the
        /// durable log without re-reading + re-measuring the text — the
        /// recall footprint is a cost dimension (how much memory context the
        /// runner attaches each turn). Old logs deserialize to 0.
        #[serde(default)]
        bytes: u32,
    },
    /// A per-turn listing of model-invocable skills, injected as a user-role
    /// system-reminder attachment at the conversation tail (the same seam as
    /// memory-recall). Carries only descriptions — the full body loads on
    /// demand when the model calls the Skill tool, so the listing stays
    /// terse (progressive disclosure). The checkpoint planner disposes this
    /// as Summarized so compaction folds it out of the assembled context;
    /// the turn-entry step scans the assembled context for a surviving listing
    /// and skips when one exists, so the listing re-surfaces after a
    /// compaction with no provider-side clear — the same natural-reset
    /// pattern memory-recall uses.
    #[serde(rename = "SkillListing")]
    SkillListing {
        text: String,
        /// The listing payload size in bytes (text.len()), recorded inline
        /// so the cost dimension (how much skill-discovery context the
        /// runner attaches) is queryable from the log without re-reading
        /// the text. Old logs deserialize to 0.
        #[serde(default)]
        bytes: u32,
        /// Fingerprint of the announced descriptor set (name, description,
        /// when-to-use, token estimate). Two listings with the same hash need
        /// no re-announce, so a turn skips when the surviving listing's hash
        /// matches the current set. 0 in old logs, which re-inject.
        #[serde(default)]
        content_hash: u64,
    },
    /// A durable record of an invoked skill's body, appended alongside the
    /// MetaUser the model reads as a directive. Unlike the MetaUser (which
    /// compaction folds into a summary, dropping the directive), SkillBody
    /// is excluded from the folded span and revived post-compact, so an
    /// invoked skill's guidance survives a compaction boundary. The
    /// untrusted flag marks bodies from non-managed/user sources so the
    /// assembly can frame them; agent_id is None on the host and set when
    /// a forked child invoked the skill. The append ts is the invoke time.
    #[serde(rename = "SkillBody")]
    SkillBody {
        skill_name: String,
        content: String,
        #[serde(default)]
        agent_id: Option<AgentId>,
        #[serde(default)]
        untrusted: bool,
    },
    AssistantMessage {
        text: String,
        /// Reasoning text that preceded this assistant message, if any. The
        /// raw Reasoning events stay in the log for replay fidelity; this
        /// field gives a reader a single-source view of the thinking attached
        /// to the message so a host can render a collapsed block
        /// without scanning for sibling Reasoning events. None when the
        /// model emitted no reasoning.
        #[serde(default)]
        thinking: Option<String>,
    },
    /// One incremental chunk of an assistant message streamed token-by-token.
    /// The durable audit trail of a streamed turn: N deltas land during the
    /// stream, then one authoritative AssistantMessage (the full text) lands at
    /// turn end. The assembly skips deltas (they are subsumed by the
    /// AssistantMessage) so the model-input history never double-counts; the
    /// deltas exist for live
    /// display and mid-stream resumability, not for replay into the model.
    AssistantTextDelta {
        text: String,
    },
    ToolCall {
        /// The id the matching ToolResult answers. A model-issued call carries
        /// the provider's id (for example toolu_01...); a command the user
        /// typed carries one the host derives from the log entry that
        /// recorded it. Unique within the log, and not an EventId.
        call_id: String,
        tool: String,
        input: serde_json::Value,
    },
    ToolResult {
        /// The call_id this result answers (matches a ToolCall.call_id).
        call_id: String,
        output: serde_json::Value,
        /// Wall-clock duration of the tool call this result answers, in ms.
        /// Inline, not in a separate file, so /trajectory's latency dimension
        /// survives
        /// resume + export + the self-evolution loop's re-reads; old logs
        /// deserialize to 0. Zero when the host did not time the call.
        #[serde(default)]
        duration_ms: u64,
    },
    /// Per-turn LLM usage + model, recorded once per turn for the trajectory
    /// cost + cache dimensions. Inline primitive fields (not the provider
    /// Usage type) keep this wire type self-contained + the context crate a
    /// leaf; the agent loop converts Usage to these at the append boundary.
    /// turn + call_in_turn carry the logical turn number + the round-trip
    /// index within it so /trajectory can render "the turn with its retry count" by grouping on turn
    /// without deriving boundaries from the event stream. Old logs
    /// deserialize with zeros / false (serde default).
    #[serde(rename = "TurnUsage")]
    TurnUsage {
        #[serde(default)]
        turn: u32,
        #[serde(default)]
        call_in_turn: u32,
        #[serde(default)]
        input_tokens: u64,
        #[serde(default)]
        output_tokens: u64,
        #[serde(default)]
        cache_read_input_tokens: u64,
        #[serde(default)]
        cache_write_input_tokens: u64,
        #[serde(default)]
        reasoning_tokens: u64,
        #[serde(default)]
        model: String,
        /// True when this call was a length-recovery continuation — the prior
        /// call hit max_tokens and the loop re-called to pick up mid-thought.
        /// Lets /trajectory + the self-evolution loop isolate retry cost
        /// per-call without correlating against TruncationVerdict. False for
        /// the terminal (non-retry) call. Old logs deserialize to false.
        #[serde(default)]
        recovery: bool,
        /// The effort level actually sent on this call (low/medium/high today;
        /// xhigh/max land here when a dialect offers them), or None when no
        /// None when no effort parameter was sent (the model does not support
        /// it, the user left it on auto, or an old log predates the field).
        /// Inlined as a string rather than the typed enum so the context crate
        /// stays a serde-only leaf with no protocol dependency; the wire form
        /// matches the enum's lowercase serde so a typed caller round-trips
        /// through the same bytes. Unknown is None, never a fake default.
        #[serde(default)]
        effort: Option<String>,
        /// Whether this call's endpoint reported the inclusive input total.
        /// A tally rebuilt from the log folds each turn's completeness from
        /// these flags instead of trusting a count that went unreported to be
        /// a genuine zero. Defaults to false so a log that predates the field
        /// reads as unknown; the other two default the same way.
        #[serde(default)]
        input_reported: bool,
        /// Whether this call's endpoint reported the cache-read count.
        #[serde(default)]
        cache_read_reported: bool,
        /// Whether this call's endpoint reported the cache-write count.
        #[serde(default)]
        cache_write_reported: bool,
        /// Whether the build that wrote this record also records what its
        /// assemblies carried from the block store. False on a log written
        /// before that record existed, where a reader must report the model
        /// view as unknown rather than infer zero blocks.
        #[serde(default)]
        cas_recorded: bool,
    },
    /// A reward-loop observation: redundant + blind-retry counts flagged this
    /// batch. Durable so a later dream can scan cross-session reward trends
    /// from the trajectory log without re-deriving from the in-memory tracker
    /// (which is process-scoped and lost on exit). Old logs deserialize to 0.
    #[serde(rename = "RewardObservation")]
    RewardObservation {
        #[serde(default)]
        redundant: u32,
        #[serde(default)]
        retry_after_error: u32,
    },
    /// A hook signal recorded for audit and self-evolution. Covers every
    /// verdict except a bare Allow, which is derivable from absence. No
    /// HookSignal for a call means every configured hook allowed it. verdict
    /// is the effective control-flow result; a hook error surfaces as Deny
    /// with its cause in error separately, so a fault and a policy decision
    /// never share a field. hook_name attributes the signal; tool_name is None
    /// for non-tool events; turn and call_in_turn are None outside a
    /// round-trip; triggered_event is Some only for a Trigger verdict.
    #[serde(rename = "HookSignal")]
    HookSignal {
        #[serde(default)]
        event: HookEventKind,
        #[serde(default)]
        verdict: HookVerdictKind,
        #[serde(default)]
        error: Option<HookErrorKind>,
        #[serde(default)]
        reason: String,
        #[serde(default)]
        hook_name: String,
        #[serde(default)]
        tool_name: Option<String>,
        #[serde(default)]
        triggered_event: Option<HookEventKind>,
        #[serde(default)]
        turn: Option<u32>,
        #[serde(default)]
        call_in_turn: Option<u32>,
    },
    Reasoning {
        text: String,
    },
    /// Marks a context boundary; the checkpoint holds the plan + summary.
    /// The token counts bracket the fold: what the selected transcript
    /// measured before it and what it measures after, so a reader can see how
    /// much the compaction actually reclaimed. Old logs carry neither and
    /// deserialize to zero.
    CompactionBoundary {
        checkpoint: CheckpointId,
        #[serde(default)]
        pre_tokens: u64,
        #[serde(default)]
        post_tokens: u64,
    },
    /// A compaction summary of the Summarized events (raw events stay in log).
    Summary {
        text: String,
    },
    /// An approval card was shown to the user. call_id links to the ToolCall
    /// the card asks about. A card the user answers is followed by a
    /// PermissionDecision; a card cancelled with Esc or abandoned at a
    /// disconnect leaves no verdict, so the ask alone is what keeps that
    /// outcome on the audit trail instead of vanishing with the connection.
    /// Audit-only: nothing acts on it, and the audit log lists it beside the
    /// verdict it pairs with.
    PermissionAsked {
        call_id: String,
        tool: String,
    },
    /// A durable permission verdict: the human approved or denied a tool call.
    /// call_id links to the ToolCall the verdict answers. scope is the lifetime
    /// the answer kept (once / session / workspace), so the audit trail records
    /// not just the yes/no but how far the consent reaches. This event
    /// is audit-only: readers skip it (the approval popup + its resolution
    /// are already visible in the transcript; this is the durable record).
    PermissionDecision {
        call_id: String,
        tool: String,
        verdict: PermissionVerdict,
        scope: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        validator: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A turn was interrupted mid-flight (process crash or forced abort) and is
    /// about to be re-driven from the durable log. Lands as the first event of
    /// the recovery turn so a replay sees the boundary between the partial turn
    /// and the regenerated one — the partial deltas are never silently
    /// concatenated with the regenerated content. Readers render it as a
    /// visible notice so the user knows the prior turn was regenerated.
    TurnAborted {
        /// A human-readable reason: process crash, forced abort, etc.
        reason: String,
    },
    /// The terminal record of one turn: the model answered, handed off, or the
    /// loop stopped. It marks where the turn ended, which nothing else in the
    /// log does (a turn holds many assistant messages, one per tool iteration,
    /// and messages also arrive inside a turn). ms is the turn's drive legs
    /// summed, measured where those loops ran, so a run paused for approval
    /// and later resumed records both legs and not the wait between them; a
    /// turn that ended without a loop (an abort while paused) records no
    /// duration rather than a claimed zero. Milliseconds so a turn shorter
    /// than a second stays distinguishable from no time at all. Durable so a
    /// frontend labels the turn with the duration it took, after a replay as
    /// well as live: a session loaded from this log shows how long each turn
    /// reasoned instead of only the turn the process happened to watch.
    #[serde(rename = "RunCompleted")]
    RunCompleted {
        #[serde(default)]
        ms: Option<u64>,
        /// How the turn ended. Absent in logs written before the outcome was
        /// recorded, where the only honest reading is that the turn closed
        /// without a stated result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<RunCompletionStatus>,
    },
    /// The per-turn truncation verdict. Persisted so the session log carries
    /// trajectory-grade data: every completed turn and every recovery
    /// iteration records whether the provider cut the reply at the token cap,
    /// which signal detected it, the raw and normalized finish reason, the
    /// token counts (server-reported and self-counted), and how many recovery
    /// attempts the drive loop made. Three emission points: when recovery
    /// fires (recovery_fired true), when recovery is exhausted, and on a
    /// clean success — so a replay sees one verdict per turn plus one per
    /// recovery iteration.
    TruncationVerdict {
        /// The provider's finish_reason before dialect normalization
        /// (max_tokens, MAX_TOKENS, length, stop) — the raw dialect the
        /// provider spoke, preserved so trajectory analysis can see which
        /// gateway spelling triggered the cut.
        raw_finish_reason: Option<String>,
        /// The finish_reason after dialect normalization (length / stop / ...).
        normalized_reason: Option<String>,
        /// Which silent-truncation signal fired, or None when the provider
        /// signaled the cap-cut directly.
        signal: TruncationSignal,
        /// Server-reported output tokens for the turn. Zero when the provider
        /// did not report usage (common for streaming proxies).
        server_output_tokens: u32,
        /// Self-counted output tokens via the shared Tokenizer. Cross-checks
        /// the server count and covers the case where the proxy omitted usage.
        self_count_output_tokens: u32,
        /// The configured max_output_tokens cap for the turn.
        max_output_tokens: u32,
        /// How many length-recovery attempts the drive loop made so far.
        recovery_attempts: u32,
        /// Whether this verdict records a recovery that fired (true) or a
        /// terminal state (false — recovery exhausted or clean success).
        recovery_fired: bool,
    },
    /// A worktree session began (EnterWorktree). The cwd + sandbox fence
    /// narrowed to the worktree path at this point; a replay that consumes
    /// the event re-narrows to restore the execution environment (resume
    /// consumption is a separate, deferred task — the record lands now so
    /// the log is reproducible, never silent on the switch).
    #[serde(rename = "WorktreeEnter")]
    WorktreeEnter {
        slug: String,
        path: String,
        branch: String,
        /// The HEAD commit at enter (baseline for change-counting on exit).
        head_commit: Option<String>,
    },
    /// A worktree session ended (ExitWorktree). action is keep (worktree +
    /// branch preserved), remove (both deleted), remove-incomplete (the
    /// directory was deleted and the branch with its creation record was
    /// kept), or cleanup-pending (the registration is gone and the directory
    /// with its branch was kept, recorded so a later removal finishes it). A
    /// replay re-narrows then restores, mirroring enter.
    #[serde(rename = "WorktreeExit")]
    WorktreeExit {
        action: String,
        path: String,
    },
    /// A prompt-cache break detected at the agent loop (after a provider
    /// response). Carries the attributed cause: "compact", "model-switch",
    /// "system-prompt-changed", "tools-changed", "retention-demoted",
    /// "ttl-expired", or "unknown". Recorded when cache_read dropped sharply
    /// vs the previous turn. Provider-agnostic (runs in the agent loop, not
    /// in a provider-specific adapter), surfacing the attribution to the user
    /// via /trajectory rather than keeping it internal. A resumed session can
    /// name only the idle gap until its first response records a prefix hash
    /// in this process.
    CacheBreak {
        cause: String,
    },
    /// The frozen per-block retention decisions the assembly used, recorded
    /// whenever they change. A resumed session restores them so its first
    /// request reuses the bytes the previous process sent. The model name
    /// lets a reader discard a map written before a model switch.
    #[serde(rename = "CacheDecisions")]
    CacheDecisions {
        #[serde(default)]
        decisions: Vec<CachedBlockDecision>,
        #[serde(default)]
        model: String,
    },
    /// The forms one assembly carried from the block store. Written once the
    /// turn's usage lands, so a replay states what the request carried
    /// without re-running the retention decisions. The unit names what the
    /// token figures count in, so a reader labels a byte count as one.
    #[serde(rename = "CasModelView")]
    CasModelView {
        #[serde(default)]
        assembly_id: u64,
        #[serde(default)]
        views: Vec<CachedModelView>,
        #[serde(default)]
        estimate_unit: String,
    },
    /// Reads of stored results this session issued, written by the reader at
    /// the read so the record names the session that asked. It lands before
    /// the tool result that answers the read, because the bytes are in hand
    /// first and the answer is assembled after. The log states what the model
    /// read, not only what the request carried.
    #[serde(rename = "CasRead")]
    CasRead {
        #[serde(default)]
        reads: Vec<CachedRead>,
    },
    /// A parent runner spawned a sub-agent. The durable boundary so a parent
    /// log replay reconstructs the spawn and its config; the child's turns live
    /// in the child's own sidechain log. Fields are wire-stable primitives. The
    /// runtime serializes its typed isolation and policy to strings at append.
    #[serde(rename = "SubagentSpawn")]
    SubagentSpawn {
        child_session_id: String,
        subagent_type: String,
        #[serde(default)]
        prompt_summary: String,
        #[serde(default)]
        isolation: String,
        #[serde(default)]
        policy: String,
        /// What initiated the spawn. The string model: plus a call id marks a
        /// delegation; system: plus a hook name marks a service or gate. Empty on
        /// older events, where a reader attributes the spawn to the nearest unclaimed call.
        #[serde(default)]
        trigger_source: String,
        /// Whether the child runs in the background or blocks the parent; read
        /// at spawn so replay never infers it from result and return order.
        #[serde(default)]
        run_in_background: bool,
    },
    /// A spawned sub-agent returned. The status string drives retry policy
    /// (timeout retries, killed does not); summary is the inline result the
    /// parent model reads; result_ref points at the child log for the full
    /// transcript; the usage fields mirror TurnUsage so the parent can
    /// reattribute the child's token cost without importing the provider
    /// Usage type (keeps context a leaf). The three reported flags mirror
    /// UsageProvenance for the same leaf reason: the parent folds the child's
    /// cache-account completeness into its own rather than re-deriving it.
    #[serde(rename = "SubagentReturn")]
    SubagentReturn {
        child_session_id: String,
        status: String,
        #[serde(default)]
        summary: String,
        #[serde(default)]
        result_ref: String,
        #[serde(default)]
        input_tokens: u64,
        #[serde(default)]
        output_tokens: u64,
        #[serde(default)]
        cache_read_input_tokens: u64,
        #[serde(default)]
        cache_write_input_tokens: u64,
        #[serde(default)]
        reasoning_tokens: u64,
        /// Whether the child's endpoint reported the inclusive input total.
        /// Defaults to false so a log that predates the field reads as unknown
        /// rather than as a complete zero; the other two flags default the
        /// same way.
        #[serde(default)]
        input_reported: bool,
        /// Whether the child's endpoint reported the cache-read count.
        #[serde(default)]
        cache_read_reported: bool,
        /// Whether the child's endpoint reported the cache-write count.
        #[serde(default)]
        cache_write_reported: bool,
        /// Tool calls executed inside the child session across its full
        /// trajectory, or 0 on logs that predate this field.
        #[serde(default)]
        tool_calls: u32,
        /// Semantic tool failures inside the child session across its full
        /// trajectory.
        #[serde(default)]
        tool_failures: u32,
        /// Wall-clock span of the child session in milliseconds.
        #[serde(default)]
        duration_ms: u64,
        /// Whether the writer measured the child's tool counts and span. A
        /// return written for a child whose log could not be read carries
        /// zeroes it did not measure, and a reader counts those as unknown
        /// rather than as a child that ran no tools. Defaults to true: a
        /// return written before the field existed carried the figures its
        /// writer measured, so an absent field must not discard them.
        #[serde(default = "measured_by_default")]
        ledger_measured: bool,
        /// Whether a restart scan wrote this return for a delegation an
        /// earlier host left open. Neither the status nor the summary can say
        /// so: both carry text that a live ending or the child's own answer
        /// can also produce, and only this flag records who wrote the
        /// boundary.
        #[serde(default)]
        orphaned: bool,
    },
    /// This session was delegated to by the named parent: the child side of
    /// the delegation boundary, written as the child session's first durable
    /// record. The parent records the same act as SubagentSpawn; the child
    /// carries it in its own log so its lineage outlives a missing descriptor.
    #[serde(rename = "ChildDelegated")]
    ChildDelegated {
        parent_session_id: String,
        subagent_type: String,
    },
    /// A sub-agent notification injected into the parent's context at a turn
    /// boundary. child_session_id pins which child finished; summary is the
    /// text the parent model reads (subagent type + terminal status + the
    /// child's own result line) so the parent can act on the result without
    /// re-reading the child transcript. turn + order are best-effort anchors
    /// for replay ordering; the event-log order is authoritative (as with
    /// SubagentReturn), so a zero is acceptable when the caller lacks the
    /// counters. topic names why the notification exists, in the vocabulary of
    /// NotificationTopic; child_session_id names what produced it, which is a
    /// child session for a subagent and a task number for a background command.
    #[serde(rename = "NotificationInjected")]
    NotificationInjected {
        child_session_id: String,
        turn: u32,
        order: u32,
        topic: String,
        #[serde(default)]
        summary: String,
        /// The stored raw output the summary names, when the producer kept
        /// any above the model's bound. The reclaim pass reads the addresses
        /// from the durable record, so a block named only by a value that
        /// never reached the log would be released while the summary still
        /// points the model at it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capture: Option<serde_json::Value>,
    },
    /// Timing for one model call, recorded by the agent loop for latency
    /// analysis. The round-trip index is 1-based, matching TurnUsage. ttft and
    /// decode are absent when the stream produced no first token (an aborted
    /// or failed call), and total is the whole request wall time either way.
    #[serde(rename = "ModelStepTiming")]
    ModelStepTiming {
        turn: u32,
        step: u32,
        total_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ttft_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decode_ms: Option<u64>,
        /// Time the stream spent inside its reasoning blocks, summed over the
        /// blocks it closed. None when it closed none (the provider sent no
        /// reasoning, or the call ended inside one): an unfinished block is not
        /// a measurement, and zero would claim the model thought for no time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_ms: Option<u64>,
        /// Time the stream spent inside its output blocks (reply text and tool
        /// call arguments), summed the same way.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_ms: Option<u64>,
    },
    /// Recorded when the user clears the conversation context. prior_turn is
    /// the model-call turn count at the moment of the clear, so a replay can
    /// place the boundary without re-deriving it from event order.
    #[serde(rename = "ContextCleared")]
    ContextCleared {
        prior_turn: u32,
    },
    /// Written before a recap request leaves the host, so a crash cannot cause
    /// the same work to be summarized and billed twice on the next start. The
    /// pair of ids is the dedup key: the last user event the attempt is about
    /// and the event its input snapshot ended at. An attempt with no later
    /// AwayRecap has an outcome this log cannot state — not a failure.
    #[serde(rename = "RecapAttempt")]
    RecapAttempt {
        last_user_event: EventId,
        as_of_event: EventId,
        source: RecapSource,
        model: String,
    },
    /// A recap the host wrote while the user was away, or when an idle session
    /// was reopened: a bounded model summary of where the work stands. The text
    /// is display-only — model input assembly skips it, so it can never act as
    /// a user instruction or as evidence that work finished.
    #[serde(rename = "AwayRecap")]
    AwayRecap {
        text: String,
        last_user_event: EventId,
        as_of_event: EventId,
        source: RecapSource,
        model: String,
        /// Absent when the endpoint reported nothing about the request.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<RecapUsage>,
    },
    /// An event kind this build does not know (written by a newer version).
    /// Lets an old binary read a newer log instead of failing the whole
    /// session read on one unrecognized line. A truly corrupt line also
    /// deserializes to Unknown rather than raising Corrupt, so callers
    /// should count Unknown events and surface the count.
    #[serde(other)]
    Unknown,
}

impl SessionEvent {
    /// Build a ToolResult with zero duration (the host did not time the call).
    /// Centralizes the field set so adding fields does not churn every call
    /// site; the agent loop passes a real duration_ms when timing wires.
    pub fn tool_result(call_id: impl Into<String>, output: serde_json::Value) -> Self {
        Self::ToolResult {
            call_id: call_id.into(),
            output,
            duration_ms: 0,
        }
    }
}

#[cfg(test)]
mod tests;
