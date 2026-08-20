//! The context layer: the ContextBackend storage
//! interface, the TurnEvent wire types, and context-window assembly.
//!
//! The append-only event log is the source of truth; compaction is
//! view-selection, not destruction — the raw log is never mutated. This crate
//! owns the storage INTERFACE (ContextBackend) and the wire types that cross
//! to backends. Backends (InMemory, LocalFile, sqlite, cloud) live in the
//! memory layer and impl ContextBackend — memory depends on this crate,
//! not the reverse (dependency inversion: the interface lives in the calling
//! layer).
//!
//! Disentangled from MemoryProvider (the semantic-recall interface, in the
//! memory layer): ContextStore keeps full-fidelity history; MemoryProvider
//! extracts facts. ContextStore makes MemoryProvider's lossy extraction safe
//! because the raw transcript is never destroyed.
//!
//! Engine-facing replay, hash-chain computation, and view assembly
//! (SessionStore wrapping a ContextBackend) live in the session layer.

use serde::{Deserialize, Serialize};
use ulid::Ulid;
use uuid::Uuid;

pub mod hook_types;
pub mod memory_types;
pub mod sandbox_types;
pub use hook_types::*;
pub use memory_types::*;
pub use sandbox_types::*;

/// The storage interface (ContextBackend trait + ContextError) lives in its
/// own module so the wire types here and the trait stay under the size gate.
pub mod backend;
pub use backend::*;

/// The session metadata sidecar (SessionMeta + SessionMetaStore trait). The
/// per-session descriptor written alongside the event log at <sid>/session.json.
pub mod meta;
pub use meta::*;

/// The compaction plan types (Disposition, TurnGroup, CheckpointManifest,
/// ContextSnapshot). Split out so the crate root stays under the size gate.
pub mod plan;
pub use plan::*;

/// A session id. UUID v4 (hyphenated). Not monotonic (unlike EventId) -- the ordering
/// invariant lives in the per-session event log, not the id, so a
/// collision-resistant random id is the right shape. sid-keyed layout
/// (<sid>/log.jsonl) uses the hyphenated Display form as the dir segment.
/// Deserialize is tolerant of a legacy ULID string (pre-change exports) so
/// an old session log resumes after the sid-format change; the ULID's 128
/// bits are reinterpreted as a Uuid. Serialize is always the hyphenated
/// UUID form (forward format only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    /// Mint a fresh session id (UUID v4).
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl SessionId {
    /// Parse a display string back into a SessionId. Accepts a hyphenated
    /// UUID (the forward format) or a legacy ULID (pre-change exports);
    /// both are 128 bits, so the ULID is reinterpreted as a Uuid. Used by
    /// the resume path to rehydrate a session from its sid.
    pub fn from_display_string(s: &str) -> Option<Self> {
        if let Ok(u) = s.parse::<Uuid>() {
            return Some(SessionId(u));
        }
        s.parse::<Ulid>()
            .ok()
            .map(|u| SessionId(Uuid::from_u128(u.into())))
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        SessionId::from_display_string(&s).ok_or_else(|| {
            serde::de::Error::custom(format!("session id is neither a UUID nor a ULID: {s}"))
        })
    }
}

/// An event id. ULID-backed; monotonic within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(Ulid);

impl EventId {
    /// Mint a fresh event id.
    pub fn new() -> Self {
        Self(Ulid::generate())
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl EventId {
    /// Parse a display string (ULID) back into an EventId.
    pub fn from_display_string(s: &str) -> Option<Self> {
        s.parse::<Ulid>().ok().map(Self)
    }
}

/// A checkpoint id (a compaction plan + summary snapshot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CheckpointId(Ulid);

impl CheckpointId {
    /// Mint a fresh checkpoint id.
    pub fn new() -> Self {
        Self(Ulid::generate())
    }
}

impl Default for CheckpointId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl CheckpointId {
    /// Parse a display string (ULID) back into a CheckpointId.
    pub fn from_display_string(s: &str) -> Option<Self> {
        s.parse::<Ulid>().ok().map(Self)
    }
}

/// A content-addressed block hash (SHA-256, hex). CAS dedup keys large tool
/// outputs / file blobs out of the in-context view while keeping them
/// retrievable. The interface is defined here; v0 backends return Unsupported.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlockHash(pub String);

/// A 32-byte hash linking one event to the previous (tamper-evidence spine).
/// None on the first event of a session. The caller (SessionStore) computes
/// the chain; the backend stores it verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrevHash(pub [u8; 32]);

/// One record in the append-only session log. The wire type stored by a
/// ContextBackend. Replay walks events in append order; the tool_use/tool_result
/// pair invariant (a ToolResult links back to its ToolCall via call_id) must
/// survive every cut — a replay or view that orphans one half is a bug.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnEvent {
    pub id: EventId,
    pub session: SessionId,
    pub ts: u64,
    pub prev_hash: Option<PrevHash>,
    pub kind: TurnEventKind,
}

/// The outcome of a human permission verdict on a tool call. The durable
/// companion to the transient approval popup: every approve / deny decision is
/// appended to the session log so the audit trail is replayable and queryable
/// (logging verdicts only in-memory would leave an audit gap — a denied
/// destructive call would leave no durable trace; appending to the log
/// closes this).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionVerdict {
    Approved,
    Denied,
}

impl PermissionVerdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Denied => "denied",
        }
    }
}

/// Which silent-truncation signal fired for a turn. The drive loop computes
/// this once from the folded turn state so it both synthesizes the cap-cut
/// finish_reason and records the cause in the verdict, without re-running
/// the heuristic. Stays None when the provider signaled the cut directly —
/// the raw finish_reason already carries the cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TruncationSignal {
    /// Server-reported output_tokens reached the max cap (within slack).
    ServerUsageNearCap,
    /// Self-counted output tokens (the shared Tokenizer) reached the max cap
    /// (within slack). Fires when the server did not report usage — common
    /// for streaming proxies that omit the usage block.
    SelfCountNearCap,
    /// An odd triple-backtick count means a code fence opened but never
    /// closed — the cut landed mid-block.
    UnclosedCodeBlock,
    /// No silent-truncation signal fired.
    None,
}

/// The payload of a TurnEvent. Tagged for JSONL. ToolResult carries call_id
/// linking to the ToolCall it answers — the pair invariant. PermissionDecision
/// carries the call_id of the tool call the verdict answers, closing the audit
/// gap where a verdict lived only in the transient transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TurnEventKind {
    UserInput {
        text: String,
    },
    /// A logical turn boundary. Appended at the model-call entry (the same
    /// seam as the in-memory start_turn) so every turn — including cancelled
    /// / errored ones that never reach a TurnUsage — carries a durable
    /// boundary marker. The projection groups turns on this, NOT on
    /// UserInput (a single prompt spans N turns of tool iterations; grouping
    /// on UserInput flattens them and hides the retry/iteration work). turn
    /// is the logical turn number; call_in_turn is 0 at the boundary (the
    /// per-round-trip index lives on TurnUsage). Old logs have no such event
    /// — the projection falls back to UserInput grouping for them.
    #[serde(rename = "TurnStarted")]
    TurnStarted {
        #[serde(default)]
        turn: u32,
        #[serde(default)]
        call_in_turn: u32,
    },
    /// A user-role message the runner injects for control purposes (e.g. the
    /// "resume directly" nudge after a token-cap truncation), not authored by
    /// the human. Served to the model like UserInput, but hosts skip it when
    /// projecting to the readable transcript so the user never sees the nudge.
    #[serde(rename = "MetaUser")]
    MetaUser {
        text: String,
    },
    /// A user message the human queued while a run was in flight, drained +
    /// appended at the next turn boundary (Path A — mid-turn injection).
    /// The durable text is the user's bare input (the transcript shows it
    /// verbatim, like UserInput). The model-input projection wraps it with a
    /// framing note so the model reads it as a mid-work interjection
    /// ("continue your current task and address it"), not a fresh
    /// instruction that drops the in-flight task.
    #[serde(rename = "MidTurnInput")]
    MidTurnInput {
        text: String,
    },
    /// A per-turn recall of memory entries the runner injects as a user-role
    /// system-reminder attachment at the conversation tail, not authored by
    /// the human. The keys field carries the surfaced memory file stems so the
    /// next turn recall can dedup-scan the transcript. The checkpoint
    /// planner disposes this as Summarized so compaction folds it out of the
    /// live projection, which makes surfaced scanning naturally reset with
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
    AssistantMessage {
        text: String,
        /// Reasoning text that preceded this assistant message, if any. The
        /// raw Reasoning events stay in the log for replay fidelity; this
        /// field gives a projection a single-source view of the thinking
        /// attached to the message so a host can render a collapsed block
        /// without scanning for sibling Reasoning events. None when the
        /// model emitted no reasoning.
        #[serde(default)]
        thinking: Option<String>,
    },
    /// One incremental chunk of an assistant message streamed token-by-token.
    /// The durable audit trail of a streamed turn: N deltas land during the
    /// stream, then one authoritative AssistantMessage (the full text) lands at
    /// turn end. Projection skips deltas (they are subsumed by the AssistantMessage)
    /// so the model-input history never double-counts; the deltas exist for live
    /// display and mid-stream resumability, not for replay into the model.
    AssistantTextDelta {
        text: String,
    },
    ToolCall {
        /// The provider-generated tool call id (e.g. "toolu_01..."). Links to
        /// the matching ToolResult. Not an EventId — the model minted it.
        call_id: String,
        tool: String,
        input: serde_json::Value,
    },
    ToolResult {
        /// The call_id this result answers (matches a ToolCall.call_id).
        call_id: String,
        output: serde_json::Value,
        /// Wall-clock duration of the tool call this result answers, in ms.
        /// Inline (not a sidecar) so /trajectory's latency dimension survives
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
        /// The effort level actually sent on this call (low/medium/high), or
        /// None when no effort parameter was sent (the model does not support
        /// it, the user left it on auto, or an old log predates the field).
        /// Inlined as a string rather than the typed enum so the context crate
        /// stays a serde-only leaf with no protocol dependency; the wire form
        /// matches the enum's lowercase serde so a typed caller round-trips
        /// through the same bytes. Unknown is None, never a fake default.
        #[serde(default)]
        effort: Option<String>,
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
    /// A hook signal recorded for audit + self-evolution. Covers every
    /// verdict EXCEPT bare Allow — Allow is derivable from absence (no
    /// HookSignal for a tool call ⟹ every configured hook allowed; the
    /// config is known, so Allow carries no non-derivable information and
    /// recording it would only scale volume with tool-call count). verdict
    /// is the EFFECTIVE control-flow result (fail-closed: a hook error
    /// surfaces here as Deny), error carries the CAUSE separately so the
    /// two never share a field (Some(error) ⟹ the Deny came from a fault,
    /// not a policy decision). hook_name attributes the signal to the hook
    /// that produced it; tool_name is None for non-tool events
    /// (SessionStart/UserPromptSubmit/...); turn/call_in_turn are None when
    /// the signal sits outside a round-trip context. triggered_event is Some
    /// only for a Trigger verdict. Old logs deserialize to defaults.
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
    CompactionBoundary {
        checkpoint: CheckpointId,
    },
    /// A compaction summary of the Summarized events (raw events stay in log).
    Summary {
        text: String,
    },
    /// A durable permission verdict: the human approved or denied a tool call.
    /// call_id links to the ToolCall the verdict answers. scope describes the
    /// breadth of the approval (once / prefix:* / session) so the audit trail
    /// records not just the yes/no but how far the consent reaches. This event
    /// is audit-only: projections skip it (the approval popup + its resolution
    /// are already visible in the transcript; this is the durable record).
    PermissionDecision {
        call_id: String,
        tool: String,
        verdict: PermissionVerdict,
        scope: String,
    },
    /// A turn was interrupted mid-flight (process crash or forced abort) and is
    /// about to be re-driven from the durable log. Lands as the first event of
    /// the recovery turn so a replay sees the boundary between the partial turn
    /// and the regenerated one — the partial deltas are never silently
    /// concatenated with the regenerated content. Projections render it as a
    /// visible notice so the user knows the prior turn was regenerated.
    TurnAborted {
        /// A human-readable reason: process crash, forced abort, etc.
        reason: String,
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
    /// branch preserved) or remove (both deleted). A replay re-narrows then
    /// restores, mirroring enter.
    #[serde(rename = "WorktreeExit")]
    WorktreeExit {
        action: String,
        path: String,
    },
    /// A prompt-cache break detected at the agent loop (after a provider
    /// response). Carries the attributed cause: "compact", "model-switch",
    /// "ttl-expiry", or "unknown". Recorded when cache_read dropped sharply
    /// vs the previous turn. Provider-agnostic (runs in the agent loop, not
    /// in a provider-specific adapter), surfacing the attribution to the user
    /// via /trajectory rather than keeping it internal.
    CacheBreak {
        cause: String,
    },
    /// An event kind this build does not know (written by a newer version).
    /// Lets an old binary read a newer log instead of failing the whole
    /// session read on one unrecognized line. A truly corrupt line also
    /// deserializes to Unknown rather than raising Corrupt, so callers
    /// should count Unknown events and surface the count.
    #[serde(other)]
    Unknown,
}

impl TurnEventKind {
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
mod tests {
    use super::*;

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
    fn test_event_serde_round_trip() {
        let s = SessionId::new();
        let e = event(
            s,
            EventId::new(),
            TurnEventKind::UserInput { text: "hi".into() },
        );
        let json = serde_json::to_string(&e).expect("serialize");
        let back: TurnEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, e);
        assert!(json.contains("\"type\":\"UserInput\""));
    }

    #[test]
    fn test_session_id_round_trips() {
        // A freshly minted SessionId serializes as a hyphenated UUID and
        // parses back to the same value, so a round trip is lossless.
        let s = SessionId::new();
        let display = s.to_string();
        assert!(
            display.len() == 36 && display.matches('-').count() == 4,
            "session id should be a hyphenated UUID, got {display}",
        );
        assert_eq!(SessionId::from_display_string(&display), Some(s));
    }

    #[test]
    fn test_session_id_accepts_ulid() {
        // A pre-change export carries ULID session ids in each event.
        // Deserialize must accept the legacy form so an old session log
        // resumes after the sid-format change; the ULID's 128 bits become
        // the Uuid's bits (value identity, not string identity).
        let legacy = "01KZ5RDH4DG6YV0EDBX1KSKTRA";
        let parsed = SessionId::from_display_string(legacy);
        assert!(parsed.is_some(), "legacy ULID should parse: {legacy}");
        let sid = parsed.unwrap();
        // Serialize is forward-only (hyphenated UUID), so the string form
        // changes on reserialize -- the value is preserved, the costume is not.
        assert_ne!(sid.to_string(), legacy);
        // The same 128 bits: round-trip the reserialized form back.
        assert_eq!(SessionId::from_display_string(&sid.to_string()), Some(sid));
        // Tolerant via the Deserialize impl too (the path export import takes).
        let json = format!("\"{legacy}\"");
        let de: SessionId = serde_json::from_str(&json).expect("deserialize legacy ULID");
        assert_eq!(de, sid);
    }

    #[test]
    fn test_session_id_rejects_garbage() {
        assert!(SessionId::from_display_string("not-a-session-id").is_none());
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
    fn test_event_variants_round_trip() {
        // Every TurnEventKind variant must survive a serde cycle: the
        // internally-tagged enum plus nested serde_json::Value and CheckpointId.
        let s = SessionId::new();
        let call_id = "toolu_01call";
        let cp = CheckpointId::new();
        let cases = vec![
            event(
                s,
                EventId::new(),
                TurnEventKind::AssistantMessage {
                    text: "hi".into(),
                    thinking: None,
                },
            ),
            event(
                s,
                EventId::new(),
                TurnEventKind::AssistantTextDelta { text: "hel".into() },
            ),
            event(
                s,
                EventId::new(),
                TurnEventKind::ToolCall {
                    call_id: call_id.to_string(),
                    tool: "edit".into(),
                    input: serde_json::json!({"path": "x.rs", "line": 3}),
                },
            ),
            event(
                s,
                EventId::new(),
                TurnEventKind::ToolResult {
                    call_id: call_id.to_string(),
                    output: serde_json::json!(["ok", 42]),
                    duration_ms: 0,
                },
            ),
            event(
                s,
                EventId::new(),
                TurnEventKind::Reasoning {
                    text: "thinking".into(),
                },
            ),
            event(
                s,
                EventId::new(),
                TurnEventKind::CompactionBoundary { checkpoint: cp },
            ),
            event(
                s,
                EventId::new(),
                TurnEventKind::Summary {
                    text: "head summarized".into(),
                },
            ),
            event(
                s,
                EventId::new(),
                TurnEventKind::PermissionDecision {
                    call_id: call_id.to_string(),
                    tool: "bash".into(),
                    verdict: PermissionVerdict::Approved,
                    scope: "once".into(),
                },
            ),
            event(
                s,
                EventId::new(),
                TurnEventKind::TruncationVerdict {
                    raw_finish_reason: Some("max_tokens".into()),
                    normalized_reason: Some("length".into()),
                    signal: TruncationSignal::ServerUsageNearCap,
                    server_output_tokens: 8_000,
                    self_count_output_tokens: 7_950,
                    max_output_tokens: 8_000,
                    recovery_attempts: 1,
                    recovery_fired: true,
                },
            ),
            event(
                s,
                EventId::new(),
                TurnEventKind::TurnUsage {
                    turn: 3,
                    call_in_turn: 2,
                    input_tokens: 1000,
                    output_tokens: 500,
                    cache_read_input_tokens: 800,
                    cache_write_input_tokens: 50,
                    reasoning_tokens: 100,
                    model: "test".into(),
                    recovery: true,
                    effort: Some("high".into()),
                },
            ),
            event(
                s,
                EventId::new(),
                TurnEventKind::HookSignal {
                    event: HookEventKind::PreToolUse,
                    verdict: HookVerdictKind::Deny,
                    error: Some(HookErrorKind::Timeout),
                    reason: "off-limits".into(),
                    hook_name: "deny-bash".into(),
                    tool_name: Some("bash".into()),
                    triggered_event: None,
                    turn: Some(3),
                    call_in_turn: Some(2),
                },
            ),
        ];
        for e in &cases {
            let json = serde_json::to_string(e).expect("serialize");
            let back: TurnEvent = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, *e);
        }
        // CompactionBoundary carries the nested CheckpointId — verify it.
        let json = serde_json::to_string(&cases[5]).unwrap();
        assert!(json.contains("\"checkpoint\""));
        let back: TurnEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cases[5]);
    }

    #[test]
    fn test_truncation_signal_round_trips() {
        for signal in [
            TruncationSignal::ServerUsageNearCap,
            TruncationSignal::SelfCountNearCap,
            TruncationSignal::UnclosedCodeBlock,
            TruncationSignal::None,
        ] {
            let json = serde_json::to_string(&signal).expect("serialize");
            let back: TruncationSignal = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, signal);
        }
    }

    #[test]
    fn test_verdict_preserves_raw_dialect() {
        // The raw finish_reason and the normalized reason must both survive a
        // serde cycle as distinct values: the raw carries the provider dialect
        // (max_tokens) while the normalized carries the flattened form (length)
        // the drive loop keys on. If the raw is lost, trajectory analysis
        // cannot tell which gateway spelling triggered the cut.
        let verdict = TurnEventKind::TruncationVerdict {
            raw_finish_reason: Some("max_tokens".into()),
            normalized_reason: Some("length".into()),
            signal: TruncationSignal::ServerUsageNearCap,
            server_output_tokens: 8_000,
            self_count_output_tokens: 0,
            max_output_tokens: 8_000,
            recovery_attempts: 2,
            recovery_fired: false,
        };
        let e = event(SessionId::new(), EventId::new(), verdict);
        let json = serde_json::to_string(&e).expect("serialize");
        assert!(json.contains("\"raw_finish_reason\":\"max_tokens\""));
        assert!(json.contains("\"normalized_reason\":\"length\""));
        let back: TurnEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, e);
        // Distinguish the two: raw is the provider dialect, normalized is the
        // flattened form. They must not collapse into one field.
        if let TurnEventKind::TruncationVerdict {
            raw_finish_reason,
            normalized_reason,
            ..
        } = back.kind
        {
            assert_eq!(raw_finish_reason.as_deref(), Some("max_tokens"));
            assert_eq!(normalized_reason.as_deref(), Some("length"));
            assert_ne!(raw_finish_reason, normalized_reason);
        } else {
            panic!("expected TruncationVerdict");
        }
    }
}

#[cfg(test)]
#[path = "turn_usage_tests.rs"]
mod turn_usage_tests;
