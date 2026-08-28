//! Request / event envelope separation. A request carries a req_id the
//! caller mints; the matching response associates by that id. An event carries
//! a globally monotonic seq; a client that drops mid-stream reports
//! resume_from: seq on reconnect so the service replays the tail. This keeps
//! requests (synchronous query/response) and events (async push stream) on
//! distinct correlation axes — a request id never collides with an event seq.

use serde::{Deserialize, Serialize};

/// A caller-minted request id. Unique within a session; the response echoes it
/// so the caller pairs reply to request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(pub u64);

/// A globally monotonic event sequence number. The service assigns these;
/// a client reconnecting reports the last seq it processed so the service
/// replays the tail without re-sending the whole stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EventSeq(pub u64);

/// A request envelope: the caller's req_id plus the request payload. The
/// frame on the wire is this envelope, not the bare payload, so the response
/// can correlate even when the transport reorders.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RequestEnvelope {
    pub req_id: RequestId,
    pub payload: crate::frontend::FrontendRequest,
}

impl RequestEnvelope {
    pub fn new(req_id: RequestId, payload: crate::frontend::FrontendRequest) -> Self {
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

/// An event envelope: the service-assigned monotonic seq plus the event
/// payload. The client tracks the highest seq it has processed for resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EventEnvelope {
    pub seq: EventSeq,
    pub payload: crate::frontend::FrontendEventKind,
}

impl EventEnvelope {
    pub fn new(seq: EventSeq, payload: crate::frontend::FrontendEventKind) -> Self {
        Self { seq, payload }
    }
}

/// The applied model the host reports back after a /model select. The model
/// id is the resolved id (a Default pick is resolved to the constant on the
/// host, so the status bar shows a real id, not a sentinel). effort is what the
/// host will actually send on the next completion; None means no effort
/// parameter is sent (the model does not support it, or the user left it on
/// auto). Carrying effort back keeps the status bar honest: the picker cannot
/// know whether the host can honor an effort for this model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelApplied {
    pub model: String,
    #[serde(default)]
    pub effort: Option<crate::llm::EffortLevel>,
}

/// One frame of a child agent's transcript, fetched on demand when the
/// parent expands a Subagent fold-group. Mirrors the live session/update +
/// acpx frame stream so the parent projects the child transcript through the
/// same pipeline as its own (the child is not a simplified list). Batched in
/// a ChildTranscript response for the sync case where the child is terminal
/// at expand time; a streaming variant for live async children is a later
/// addition and does not alter this one-shot snapshot shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "frame", content = "data", rename_all = "snake_case")]
pub enum ChildTranscriptFrame {
    /// A session/update chunk the child produced (user prompt echo, assistant
    /// message, tool call, tool-call update). The base turn stream.
    Session(crate::frontend::session_update::SessionUpdate),
    /// An acpx/* extension notification the child produced (compaction
    /// boundary, summary), carried so the child transcript is isomorphic to
    /// the parent's, not a stripped subset.
    Acpx(crate::acpx::AcpxNotification),
}

/// The payload of a response to a request. Responses sit on the req_id axis
/// (paired to their request); events sit on the seq axis. A run request returns
/// either RunOk (the turn finished) or RunErr (the run failed before an
/// outcome); other requests return Ack or a wire error when the request itself
/// is invalid for the current state.
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
    RunOk(crate::frontend::run::RunResult),
    /// A run failed before producing an outcome (provider exhausted, context
    /// error, max turns). The kind plus display string lets the frontend
    /// surface an error line.
    RunErr(crate::frontend::run::RunError),
    /// A request that needs no payloaded reply (the effect landed; events on
    /// the seq stream carry any follow-on state).
    Ack,
    /// A status snapshot the frontend requested (/status). Carries the wire
    /// form so the TUI renders without importing the engine crate.
    Status(crate::frontend::status::StatusSnapshot),
    /// The session's turn-event trajectory (/trajectory), projected to the
    /// wire session/update form so the TUI renders the trajectory without
    /// importing the engine or context crate. Kinds with no base-protocol
    /// counterpart (compaction boundary, summary) ride the acpx/context
    /// stream separately and are not in this first cut. The redundant
    /// field carries the flagged same-input re-issues (self-evolution
    /// reward signal) for a "redundant calls" section in the pane.
    Trajectory(crate::frontend::trajectory::TrajectoryResponse),
    /// A context-window breakdown (/context), projected to the wire form so
    /// the TUI renders the token-budget visualization without importing the
    /// engine or context crate.
    Context(crate::frontend::context::ContextBreakdown),
    /// A manual /compact result: whether progress was made, the folded event
    /// count, the persisted manifest id, and pre/post token estimates. The
    /// TUI renders a one-line outcome so the user knows the window shrank.
    Compact(crate::frontend::compact::CompactReply),
    /// The current permission mode (/mode), projected to the wire form so the
    /// TUI renders the mode label without importing the permission crate.
    PermissionMode(crate::frontend::permission::PermissionMode),
    /// The durable permission rule set (/rules), projected to the wire form so
    /// the TUI renders the rule list without importing the permission crate.
    PermissionRules(Vec<crate::frontend::permission::PermissionRule>),
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
    Tools(Vec<crate::frontend::tools::ToolEntry>),
    /// The /agents reply: the formatted agent directory string
    /// (registered sub-agent types, minus denied, sorted).
    Agents(String),
    /// The /hooks reply: the registered hooks (read-only visibility).
    Hooks(Vec<crate::frontend::hooks::HookEntry>),
    /// The /skills reply: the discovered skills (name, description,
    /// source, body token estimate).
    Skills(Vec<crate::frontend::skills::SkillEntry>),
    /// The /undo reply: a description of what was undone, or None when the
    /// undo stack was empty.
    UndoResult(Option<String>),
    /// The /model select reply: the model id and effort the host actually
    /// applied, so the status bar renders what is being sent rather than what
    /// the picker requested. effort None means the host is not sending an
    /// effort parameter for this model (unsupported, or auto).
    ModelResult(ModelApplied),
    /// The /model pane catalog snapshot (/model command): the active id,
    /// the global effort fallback, and the catalog rows in written order, so
    /// the host renders the pane without importing the config crate.
    ModelInfo(crate::frontend::model::ModelCatalog),
    /// The /memory list reply: every stored memory as a frontmatter-only
    /// summary (no body), so the TUI renders the index without importing the
    /// engine or context crate.
    MemoryList(Vec<crate::frontend::memory::MemorySummaryEntry>),
    /// The /memory <key> reply: the full body of one memory, or None when the
    /// key is absent or no provider is wired.
    MemoryShow(Option<crate::frontend::memory::MemoryDetail>),
    /// The /memory toggle reply: both toggles after a read or a flip, so the
    /// pane renders on/off rows without importing the config crate.
    ToggleState(crate::frontend::memory::ToggleState),
    /// The /debug reply: whether the diagnostic sink is now recording and
    /// the file path it writes to, so the host can surface both in a system
    /// line without guessing where to look.
    Debug(crate::frontend::debug::DebugState),
    /// The on-demand child transcript (expanding a Subagent fold-group): the
    /// child session's turn events projected to the same session/update +
    /// acpx frame stream the parent accumulates, so the TUI projects the
    /// child transcript through the same pipeline as its own. A sync child
    /// is terminal at expand time, so this is a one-shot full snapshot. The
    /// child_sid echoes the request so the stateless driver can route the
    /// reply to the Subagent line without a req_id-to-sid map.
    ChildTranscript {
        child_sid: crate::frontend::SessionId,
        frames: Vec<ChildTranscriptFrame>,
    },
    /// The request itself was invalid for the current state or capability.
    Error(crate::wire::WireError),
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
pub enum ServerRequestPayload {
    /// A tool needs a human verdict before the run proceeds. The turn stays
    /// live; the client answers with a permission decision on the same req_id.
    Permission(crate::frontend::run::ApprovalRequest),
    /// A one-time workspace-trust ask fired at startup before the run loop
    /// when the project source is not yet acknowledged. Distinct from
    /// Permission: trust is a property of the folder, not of an individual
    /// tool call, so it asks once and persists the answer. The client
    /// replies with a TrustAccept on the same req_id; a decline ends the
    /// session.
    TrustPrompt(crate::frontend::trust::TrustPrompt),
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
    Permission(crate::frontend::run::ApprovalDecision),
    /// The client answer to a startup TrustPrompt: accepted persists the
    /// project path as trusted in user-level settings; declined ends the
    /// session.
    TrustAccept(crate::frontend::trust::TrustAccept),
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
/// wires the reverse-request projection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClientFrame {
    Request(RequestEnvelope),
    Response(ClientResponseEnvelope),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::run::{ApprovalDecision, ApprovalRequest};
    use serde_json::Value;

    fn sample_ask() -> ApprovalRequest {
        ApprovalRequest {
            call_id: "toolu_1".into(),
            tool_name: "bash".into(),
            input: Value::Null,
            options: Vec::new(),
            reason: None,
        }
    }

    fn sample_decision() -> ApprovalDecision {
        ApprovalDecision {
            call_id: "toolu_1".into(),
            approved: true,
            updated_input: None,
            scope: "once".into(),
        }
    }

    #[test]
    fn test_request_envelope_round_trips() {
        // FrontendRequest::Console is a unit variant; safe to construct.
        let e = RequestEnvelope::new(RequestId(7), crate::frontend::FrontendRequest::Console);
        let json = serde_json::to_string(&e).expect("serialize");
        let back: RequestEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.req_id, RequestId(7));
    }

    #[test]
    fn test_event_seq_orders_monotonically() {
        assert!(EventSeq(1) < EventSeq(2));
        assert!(EventSeq(2) > EventSeq(1));
    }

    #[test]
    fn test_resume_encodes_none_some() {
        let none_json = serde_json::to_string(&ResumeFrom::from_start()).unwrap();
        let some_json = serde_json::to_string(&ResumeFrom::after(EventSeq(3))).unwrap();
        assert!(
            none_json.contains("null"),
            "from_start serializes null: {none_json}"
        );
        assert!(
            some_json.contains("3"),
            "after encodes the seq: {some_json}"
        );
    }

    #[test]
    fn test_server_request_round_trips() {
        // A reverse permission ask carries an ApprovalRequest; the req_id
        // is server-minted and the client echoes it in the reply.
        let ask = ServerRequestEnvelope::new(
            RequestId(11),
            ServerRequestPayload::Permission(sample_ask()),
        );
        let json = serde_json::to_string(&ask).expect("serialize");
        let back: ServerRequestEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.req_id, RequestId(11));
        assert!(json.contains("\"permission\""));
    }

    #[test]
    fn test_trust_prompt_envelope() {
        // A startup workspace-trust ask tags as trust_prompt (distinct from
        // the per-action permission tag), so a client dispatches it to a
        // trust card, not a permission card.
        let ask = ServerRequestEnvelope::new(
            RequestId(42),
            ServerRequestPayload::TrustPrompt(crate::frontend::trust::TrustPrompt {
                project_path: "/proj".into(),
                risks: vec![crate::frontend::trust::TrustRisk {
                    kind: "skill_bash".into(),
                    name: "commit".into(),
                }],
            }),
        );
        let json = serde_json::to_string(&ask).expect("serialize");
        let back: ServerRequestEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.req_id, RequestId(42));
        assert!(
            json.contains("\"trust_prompt\""),
            "wire tag must be trust_prompt: {json}"
        );
    }

    #[test]
    fn test_trust_accept_envelope() {
        let reply = ClientResponseEnvelope::new(
            RequestId(42),
            ClientResponsePayload::TrustAccept(crate::frontend::trust::TrustAccept {
                accepted: true,
            }),
        );
        let json = serde_json::to_string(&reply).expect("serialize");
        let back: ClientResponseEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.req_id, RequestId(42));
        assert!(
            json.contains("\"trust_accept\""),
            "wire tag must be trust_accept: {json}"
        );
    }

    #[test]
    fn test_frame_tags_request_response() {
        let req = ClientFrame::Request(RequestEnvelope::new(
            RequestId(1),
            crate::frontend::FrontendRequest::Console,
        ));
        let res = ClientFrame::Response(ClientResponseEnvelope::new(
            RequestId(2),
            ClientResponsePayload::Permission(sample_decision()),
        ));
        let req_json = serde_json::to_string(&req).unwrap();
        let res_json = serde_json::to_string(&res).unwrap();
        assert!(req_json.contains("\"request\""));
        assert!(res_json.contains("\"response\""));
    }

    #[test]
    fn test_server_frame_tags_request() {
        let req = ServerFrame::Request(ServerRequestEnvelope::new(
            RequestId(5),
            ServerRequestPayload::Permission(sample_ask()),
        ));
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"request\""));
        assert!(json.contains("\"permission\""));
    }

    #[test]
    fn test_model_set_roundtrips_field() {
        // The /model select request: model None = Default sentinel, effort
        // None = auto, effort_toggled records whether the picker touched
        // effort. All three fields survive a wire round-trip, verified by
        // re-serializing the deserialized envelope and comparing byte for
        // byte (field-level equality on the wire form).
        let req = RequestEnvelope::new(
            RequestId(9),
            crate::frontend::FrontendRequest::ModelSet {
                model: None,
                effort: Some(crate::llm::EffortLevel::High),
                effort_toggled: true,
            },
        );
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(
            json.contains(r#""model":null"#),
            "Default sentinel => null: {json}"
        );
        assert!(
            json.contains(r#""effort":"high""#),
            "effort encodes lowercase: {json}"
        );
        assert!(
            json.contains(r#""effort_toggled":true"#),
            "toggled flag: {json}"
        );
        let back: RequestEnvelope = serde_json::from_str(&json).expect("deserialize");
        let json2 = serde_json::to_string(&back).expect("reserialize");
        assert_eq!(json, json2, "round-trip preserves all three fields");
    }

    #[test]
    fn test_model_applied_roundtrips() {
        // The reply: the host resolves the sentinel to a real id and reports
        // the effort it will actually send. effort None is honest when the
        // host is not sending an effort parameter.
        let reply = ResponseEnvelope::new(
            RequestId(9),
            ResponsePayload::ModelResult(ModelApplied {
                model: "qwen3.7-max".into(),
                effort: None,
            }),
        );
        let json = serde_json::to_string(&reply).expect("serialize");
        let back: ResponseEnvelope = serde_json::from_str(&json).expect("deserialize");
        let json2 = serde_json::to_string(&back).expect("reserialize");
        assert_eq!(json, json2, "ModelApplied round-trips byte for byte");
        assert!(json.contains(r#""type":"model_result""#));
        assert!(json.contains(r#""effort":null"#));
        assert!(json.contains("qwen3.7-max"));
    }

    #[test]
    fn test_model_applied_without_effort() {
        // An older host that predates the effort field still produces a
        // ModelResult the new client can read: effort defaults to None.
        let legacy = r#"{"req_id":9,"payload":{"type":"model_result","data":{"model":"glm-5.2"}}}"#;
        let back: ResponseEnvelope = serde_json::from_str(legacy).expect("deserialize");
        let json2 = serde_json::to_string(&back).expect("reserialize");
        assert!(
            json2.contains(r#""effort":null"#),
            "missing effort defaults to None, not a decode error: {json2}"
        );
        assert!(json2.contains("glm-5.2"));
    }

    #[test]
    fn test_model_info_catalog_roundtrips() {
        // The /model pane catalog snapshot survives a wire round-trip: the
        // active id, effort fallback, and catalog entries (with display_name
        // + description + effort) all preserve.
        let reply = ResponseEnvelope::new(
            RequestId(11),
            ResponsePayload::ModelInfo(crate::frontend::model::ModelCatalog {
                active_id: Some("qwen3.7-max".into()),
                effort_level: Some(crate::llm::EffortLevel::High),
                catalog: vec![crate::frontend::model::ModelCatalogEntry {
                    id: "qwen3.7-max".into(),
                    display_name: Some("Max".into()),
                    description: Some("most capable".into()),
                    effort: Some(crate::llm::EffortLevel::Medium),
                }],
            }),
        );
        let json = serde_json::to_string(&reply).expect("serialize");
        let back: ResponseEnvelope = serde_json::from_str(&json).expect("deserialize");
        match back.payload {
            ResponsePayload::ModelInfo(catalog) => {
                assert_eq!(catalog.active_id.as_deref(), Some("qwen3.7-max"));
                assert_eq!(catalog.effort_level, Some(crate::llm::EffortLevel::High));
                assert_eq!(catalog.catalog.len(), 1);
                assert_eq!(catalog.catalog[0].display_name.as_deref(), Some("Max"));
                assert_eq!(
                    catalog.catalog[0].effort,
                    Some(crate::llm::EffortLevel::Medium)
                );
            }
            other => panic!("expected ModelInfo, got {other:?}"),
        }
    }
}
