//! Request / event envelope separation. A request carries a req_id the
//! caller mints; the matching response associates by that id. An event carries
//! a globally monotonic seq; a client that drops mid-stream reports
//! resume_from: seq on reconnect so the service replays the tail. This keeps
//! requests (synchronous query/response) and events (async push stream) on
//! distinct correlation axes — a request id never collides with an event seq.

use serde::{Deserialize, Serialize};

use crate::acpx::AcpxNotification;
use crate::error::ProtocolError;
use crate::frontend::FrontendEvent;
use crate::frontend::FrontendRequest;
use crate::frontend::SessionId;
use crate::frontend::compact::CompactReply;
use crate::frontend::context::ContextBreakdown;
use crate::frontend::debug::DebugState;
use crate::frontend::hooks::HookEntry;
use crate::frontend::memory::MemoryDetail;
use crate::frontend::memory::MemorySummaryEntry;
use crate::frontend::memory::ToggleState;
use crate::frontend::model::ModelApplyResult;
use crate::frontend::model::ModelCatalog;
use crate::frontend::permission::PermissionMode;
use crate::frontend::permission::PermissionRule;
use crate::frontend::run::ApprovalDecision;
use crate::frontend::run::ApprovalRequest;
use crate::frontend::run::RunError;
use crate::frontend::run::RunResult;
use crate::frontend::session_update::SessionUpdate;
use crate::frontend::skills::SkillEntry;
use crate::frontend::status::StatusSnapshot;
use crate::frontend::tools::ToolEntry;
use crate::frontend::trajectory::TrajectoryResponse;
use crate::frontend::trust::TrustAccept;
use crate::frontend::trust::TrustPrompt;

/// A caller-minted request id. Unique within a session; the response echoes it
/// so the caller pairs reply to request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(pub u64);

/// A globally monotonic event sequence number. The service assigns these;
/// a client reconnecting reports the last seq it processed so the service
/// replays the tail without re-sending the whole stream. Sequence numbers
/// answer push order and resume position only; they restart on every service
/// process, so content identity never derives from them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventSeq(pub u64);

/// The transport form of a host durable event identity: the ULID string of
/// the session log entry the payload was mapped from. The protocol crate
/// stays serde-only with no internal dependencies, so the identity crosses
/// the process boundary as a string newtype rather than the context crate's
/// typed id. Stable across reconnects and process restarts, unlike EventSeq.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DurableEventId(pub String);

/// A request envelope: the caller's req_id plus the request payload. The
/// frame on the wire is this envelope, not the bare payload, so the response
/// can correlate even when the transport reorders.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RequestEnvelope {
    pub req_id: RequestId,
    pub payload: FrontendRequest,
}

impl RequestEnvelope {
    pub fn new(req_id: RequestId, payload: FrontendRequest) -> Self {
        Self { req_id, payload }
    }
}

/// The seq the client has processed up to; sent on (re)attach so the service
/// resumes the event stream from the next seq rather than the whole log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeFrom(pub Option<EventSeq>);

impl ResumeFrom {
    /// A fresh attach: the client has processed nothing, replay from seq 0.
    pub fn from_start() -> Self {
        Self(None)
    }
    /// Resume after the given seq (replay from the next one).
    pub fn after(seq: EventSeq) -> Self {
        Self(Some(seq))
    }
}

/// An event envelope: the service-assigned monotonic seq, the event payload,
/// and — when the payload has a durable origin — the log entry identity.
/// The client tracks the highest seq it has processed for resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EventEnvelope {
    pub seq: EventSeq,
    pub payload: FrontendEvent,
    /// The durable identity of the log entry this payload was mapped
    /// from. Present exactly when the payload has a durable origin; runtime
    /// pushes with no log entry (stream previews, agent status, system
    /// notices) leave it absent and the consumer falls back to a local row
    /// identity. The fallback is locally visible, so no capability bit
    /// negotiates it. An optional increment: absent from the serialized
    /// form when None, and a peer that predates the field ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<DurableEventId>,
}

impl EventEnvelope {
    pub fn new(seq: EventSeq, payload: FrontendEvent) -> Self {
        Self {
            seq,
            payload,
            event_id: None,
        }
    }

    /// An envelope for one payload mapped from a durable log entry. The
    /// seq carries push order and resume position; the event_id carries the
    /// entry identity that survives reconnects and process restarts. Every
    /// payload mapped from the same entry carries the same identity.
    pub fn durable(seq: EventSeq, payload: FrontendEvent, event_id: DurableEventId) -> Self {
        Self {
            seq,
            payload,
            event_id: Some(event_id),
        }
    }
}

/// One frame of a child agent's transcript, fetched on demand when the
/// parent expands a Subagent fold-group. Mirrors the live session/update +
/// acpx frame stream so the parent maps the child transcript through the
/// same pipeline as its own (the child is not a simplified list). Batched in
/// a ChildTranscript response for the sync case where the child is terminal
/// at expand time; a streaming variant for live async children is a later
/// addition and does not alter this one-shot snapshot shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "frame", content = "data", rename_all = "snake_case")]
pub enum ChildTranscriptFrame {
    /// A session/update chunk the child produced (user prompt echo, assistant
    /// message, tool call, tool-call update). The base turn stream.
    Session(SessionUpdate),
    /// An acpx/* extension notification the child produced (compaction
    /// boundary, summary), carried so the child transcript is isomorphic to
    /// the parent's, not a stripped subset.
    Acpx(AcpxNotification),
}

/// The payload of a response to a request. Responses sit on the req_id axis
/// (paired to their request); events sit on the seq axis. A run request returns
/// either RunOk (the turn finished) or RunErr (the run failed before an
/// outcome); other requests return Ack or a protocol error when the request
/// itself is invalid for the current state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
#[expect(
    clippy::large_enum_variant,
    reason = "wire DTO: serialized, size irrelevant"
)] // wire DTO: serialized, size is irrelevant
pub enum ResponsePayload {
    /// A run completed. The outcome (final output / interruption / handoff /
    /// interrupted / verify-failed) carries the same shape the engine
    /// produced, mapped to the wire form at the service boundary.
    RunOk(RunResult),
    /// A run failed before producing an outcome (provider exhausted, context
    /// error, max turns). The category plus display string lets the frontend
    /// surface an error line.
    RunErr(RunError),
    /// A request that needs no payloaded reply (the effect landed; events on
    /// the seq stream carry any follow-on state).
    Ack,
    /// A status snapshot the frontend requested (/status). Carries the wire
    /// form so the TUI renders without importing the engine crate.
    Status(StatusSnapshot),
    /// The session's turn-event trajectory (/trajectory), mapped to the
    /// wire session/update form so the TUI renders the trajectory without
    /// importing the engine or context crate. Kinds with no base-protocol
    /// counterpart (compaction boundary, summary) ride the acpx/context
    /// stream separately and are not in this first cut. The redundant
    /// field carries the flagged same-input re-issues (self-evolution
    /// reward signal) for a "redundant calls" section in the pane.
    Trajectory(TrajectoryResponse),
    /// A context-window breakdown (/context), mapped to the wire form so
    /// the TUI renders the token-budget visualization without importing the
    /// engine or context crate.
    Context(ContextBreakdown),
    /// A manual /compact result: whether progress was made, the folded event
    /// count, the persisted manifest id, and pre/post token estimates. The
    /// TUI renders a one-line outcome so the user knows the window shrank.
    Compact(CompactReply),
    /// The current permission mode (/mode), mapped to the wire form so the
    /// TUI renders the mode label without importing the permission crate.
    PermissionMode(PermissionMode),
    /// The durable permission rule set (/rules), mapped to the wire form so
    /// the TUI renders the rule list without importing the permission crate.
    PermissionRules(Vec<PermissionRule>),
    /// The directories the user added to the sandbox workspace at runtime
    /// (/permissions Workspace tab), canonicalized. The list is the source of
    /// truth the TUI renders against; it refreshes on every add/remove so no
    /// separate poll is needed.
    PermissionWorkingDirs(Vec<String>),
    /// The git-confirm checkpoint toggle state (/permissions): whether git
    /// commit/rebase/reset/tag Ask before running.
    PermissionAskBeforeGit(bool),
    /// The registered tool list (/tools), so the TUI renders the capability
    /// inventory without importing the engine registry.
    Tools(Vec<ToolEntry>),
    /// The /agents reply: the formatted agent directory string
    /// (registered sub-agent types, minus denied, sorted).
    Agents(String),
    /// The /hooks reply: the registered hooks (read-only visibility).
    Hooks(Vec<HookEntry>),
    /// The /skills reply: the discovered skills (name, description,
    /// source, body token estimate).
    Skills(Vec<SkillEntry>),
    /// The /skills detail reply: the body text of one skill as prepared
    /// for injection (frontmatter stripped, base-dir header, substituted
    /// variables, resource manifest), or None when the skill is no longer
    /// discoverable or its body could not be read.
    SkillBody(Option<String>),
    /// The /undo reply: a description of what was undone, or None when the
    /// undo stack was empty.
    UndoResult(Option<String>),
    /// The /model select reply: the selection the host applied, the model the
    /// next request will send, when the pick takes effect, and whether it
    /// persisted. The transcript and status bar format from this rather than
    /// from the draft, so a pick that failed or only reached memory cannot
    /// render as a saved success.
    ModelResult(ModelApplyResult),
    /// The /model pane catalog snapshot (/model command): the selection, the
    /// applied model, the Default resolution, the global effort fallback and
    /// the catalog rows in written order, so the host renders the pane
    /// without importing the config crate.
    ModelInfo(ModelCatalog),
    /// The /memory list reply: every stored memory as a frontmatter-only
    /// summary (no body), so the TUI renders the index without importing the
    /// engine or context crate.
    MemoryList(Vec<MemorySummaryEntry>),
    /// The /memory <key> reply: the full body of one memory, or None when the
    /// key is absent or no provider is wired.
    MemoryShow(Option<MemoryDetail>),
    /// The /memory toggle reply: both toggles after a read or a flip, so the
    /// pane renders on/off rows without importing the config crate.
    ToggleState(ToggleState),
    /// The /debug reply: whether the diagnostic sink is now recording and
    /// the file path it writes to, so the host can surface both in a system
    /// line without guessing where to look.
    Debug(DebugState),
    /// The on-demand child transcript (expanding a Subagent fold-group): the
    /// child session's turn events mapped to the same session/update +
    /// acpx frame stream the parent accumulates, so the TUI renders the
    /// child transcript through the same pipeline as its own. A sync child
    /// is terminal at expand time, so this is a one-shot full snapshot. The
    /// child_sid echoes the request so the stateless driver can route the
    /// reply to the Subagent line without a req_id-to-sid map.
    ChildTranscript {
        child_sid: SessionId,
        frames: Vec<ChildTranscriptFrame>,
    },
    /// The request itself was invalid for the current state or capability.
    Error(ProtocolError),
}

/// A response envelope: the caller's req_id echoed back plus the response
/// payload. The frame the server sends to answer a request is this envelope,
/// not the bare payload, so the client pairs reply to request even when the
/// transport reorders relative to the push stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ResponseEnvelope {
    pub req_id: RequestId,
    pub payload: ResponsePayload,
}

impl ResponseEnvelope {
    pub fn new(req_id: RequestId, payload: ResponsePayload) -> Self {
        Self { req_id, payload }
    }
}

/// A payload the server pushes to the client as a reverse request: a query
/// the server needs answered mid-turn, correlated by req_id just like a
/// client request. The first shape is a permission ask the engine surfaces
/// while a run is still live (the turn does not end); the client replies with
/// a ClientResponseEnvelope carrying the decision. non_exhaustive so a future
/// reverse-request shape (a clarification ask, a content confirmation) lands
/// without reworking every client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
// Two-variant reverse-request payload, one alive at a time (never bulk-stored),
// so the size difference is harmless; the Permission variant carries a
// serde_json::Value tool input that is large by nature.
#[allow(clippy::large_enum_variant)]
pub enum ServerRequestPayload {
    /// A tool needs a human verdict before the run proceeds. The turn stays
    /// live; the client answers with a permission decision on the same req_id.
    Permission(ApprovalRequest),
    /// A one-time workspace-trust ask fired at startup before the run loop
    /// when the project source is not yet acknowledged. Distinct from
    /// Permission: trust is a property of the folder, not of an individual
    /// tool call, so it asks once and persists the answer. The client
    /// replies with a TrustAccept on the same req_id; a decline ends the
    /// session.
    TrustPrompt(TrustPrompt),
}

/// A reverse request the server sends to the client. The req_id is server-
/// minted; the client echoes it in its ClientResponseEnvelope so the server
/// pairs reply to ask even when the transport reorders relative to the event
/// stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ServerRequestEnvelope {
    pub req_id: RequestId,
    pub payload: ServerRequestPayload,
}

impl ServerRequestEnvelope {
    pub fn new(req_id: RequestId, payload: ServerRequestPayload) -> Self {
        Self { req_id, payload }
    }
}

/// A payload the client sends back to answer a server reverse request. The
/// shape matches the ask: a permission reply carries the decision plus any
/// input the human edited. non_exhaustive for the same reason as
/// ServerRequestPayload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClientResponsePayload {
    /// The human verdict on a permission ask, plus an optional edited input
    /// the engine re-feeds to the tool on resume.
    Permission(ApprovalDecision),
    /// The client answer to a startup TrustPrompt: accepted persists the
    /// project path as trusted in user-level settings; declined ends the
    /// session.
    TrustAccept(TrustAccept),
}

/// A reverse-response envelope the client sends to answer a server reverse
/// request. The req_id echoes the ServerRequestEnvelope the client is replying
/// to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ClientResponseEnvelope {
    pub req_id: RequestId,
    pub payload: ClientResponsePayload,
}

impl ClientResponseEnvelope {
    pub fn new(req_id: RequestId, payload: ClientResponsePayload) -> Self {
        Self { req_id, payload }
    }
}

/// A frame the server sends to the client. Either an event on the monotonic
/// push stream (seq axis), a response paired to a prior client request
/// (req_id axis), or a reverse request the server mints for the client to
/// answer mid-turn (req_id axis). Tagged so the client decodes one type and
/// routes to the right handler without guessing which axis a frame belongs to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServerFrame {
    Event(EventEnvelope),
    Response(ResponseEnvelope),
    Request(ServerRequestEnvelope),
}

/// A frame the client sends to the server. Either a request the client mints
/// (the server answers with a ResponseEnvelope) or a reverse response the
/// client sends to answer a prior ServerRequestEnvelope. Tagged so the server
/// decodes one type and routes by axis. Introduces the client-side tag the
/// reverse-request flow needs; the codec switches to it when the service
/// wires the reverse-request mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClientFrame {
    Request(RequestEnvelope),
    Response(ClientResponseEnvelope),
}

#[cfg(test)]
mod tests;
