//! The acpx/* extension surface: the protocol's own ACP-shaped extension
//! methods (serde wire types matching, no dependency on the
//! agent-client-protocol crate, which stays confined to the service layer). A client
//! opts into acpx via a capability flag at Hello; a standard client ignores
//! the unknown methods (JSON-RPC 2.0 permits), so a pure-base client is a
//! drop-in.
//!
//! The method string travels on the wire (the base protocol's ext_*
//! mechanism is string-keyed by design); the typed AcpxMethod enum lives
//! here so the string never leaks past the adapter boundary — every consumer
//! matches the typed enum, so a method this build does not know decodes as
//! Unknown instead of failing the read. The adapter (service layer) maps the
//! wire string to the enum on the way in and back on the way out.
//!
//! LlmEvent (token-level provider stream) projects onto acpx/llm/* as an
//! independent notification stream — it does NOT ride the base session/update
//! channel, which carries the turn-level SessionLogEntry projection. The two are
//! orthogonal: LlmEvent is live token flow; SessionLogEntry is the durable turn
//! record.

use crate::llm::LlmEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The typed acpx/* method namespace. Wire-serializes to the string key the
/// base protocol ext_* mechanism carries (e.g. LlmTextDelta serializes as
/// "acpx/llm/text_delta"). non_exhaustive so a new extension method lands
/// without reworking every match; a method this build does not know decodes
/// as Unknown, so a newer peer's addition costs one skipped notification
/// instead of the connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AcpxMethod {
    // acpx/llm/* — the token-level provider stream (matches LlmEvent).
    #[serde(rename = "acpx/llm/step_start")]
    LlmStepStart,
    #[serde(rename = "acpx/llm/step_finish")]
    LlmStepFinish,
    #[serde(rename = "acpx/llm/text_start")]
    LlmTextStart,
    #[serde(rename = "acpx/llm/text_delta")]
    LlmTextDelta,
    #[serde(rename = "acpx/llm/text_end")]
    LlmTextEnd,
    #[serde(rename = "acpx/llm/reasoning_start")]
    LlmReasoningStart,
    #[serde(rename = "acpx/llm/reasoning_delta")]
    LlmReasoningDelta,
    #[serde(rename = "acpx/llm/reasoning_end")]
    LlmReasoningEnd,
    #[serde(rename = "acpx/llm/tool_input_start")]
    LlmToolInputStart,
    #[serde(rename = "acpx/llm/tool_input_delta")]
    LlmToolInputDelta,
    #[serde(rename = "acpx/llm/tool_input_end")]
    LlmToolInputEnd,
    #[serde(rename = "acpx/llm/tool_call")]
    LlmToolCall,
    #[serde(rename = "acpx/llm/tool_result")]
    LlmToolResult,
    #[serde(rename = "acpx/llm/tool_error")]
    LlmToolError,
    #[serde(rename = "acpx/llm/finish")]
    LlmFinish,
    #[serde(rename = "acpx/llm/provider_error")]
    LlmProviderError,

    // acpx/max_turns — the MaxTurnsReached side channel (the run hit the
    // turn cap; the stop_reason carries max_turn_requests, the turn count
    // rides here). Lands when the reverse-request projection promotes it.
    #[serde(rename = "acpx/max_turns")]
    MaxTurns,

    // acpx/tool/progress — a long-running tool (currently bash) reports its
    // elapsed seconds so the host can show the chip is not stuck. Carries
    // call_id + elapsed_secs in params. Ephemeral like the llm deltas: a
    // later authoritative tool-result frame supersedes it.
    #[serde(rename = "acpx/tool/progress")]
    ToolProgress,

    // acpx/context/* — SessionLogEntry kinds the base session/update has no
    // standard counterpart for (CompactionBoundary, Summary, MetaUser,
    // PermissionDecision, RunCompleted). These ride the extension
    // notification stream (the durable-context audit trail), orthogonal to
    // session/update.
    #[serde(rename = "acpx/context/compaction_boundary")]
    ContextCompactionBoundary,
    #[serde(rename = "acpx/context/summary")]
    ContextSummary,
    #[serde(rename = "acpx/context/meta_user")]
    ContextMetaUser,
    #[serde(rename = "acpx/context/permission_decision")]
    ContextPermissionDecision,
    /// A run reached its terminal outcome. Carries secs in params: how long
    /// the turn's drive legs ran, summed, which is the work it spent rather
    /// than the wall clock it was open for. The host closes the turn's summary
    /// row on this frame, so a replayed session shows the same row the live
    /// one did.
    #[serde(rename = "acpx/context/run_completed")]
    ContextRunCompleted,
    /// A message delivered into a running turn rather than one that opens it.
    /// The message itself rides the user-message stream; this mark rides
    /// beside it, because the chunk a client reads cannot say which of the two
    /// it is and a turn boundary turns on that. A queued interjection and a
    /// background child's completion are both delivered this way.
    #[serde(rename = "acpx/context/mid_turn_input")]
    ContextMidTurnInput,
    /// A background child finished and its result was handed to the running
    /// turn. Marked as delivered rather than opening, by the same rule as a
    /// queued interjection.
    #[serde(rename = "acpx/context/child_completed")]
    ContextChildCompleted,
    /// A turn was interrupted (a cancel, or a process that died mid-turn) and
    /// the run regenerates inside the same user turn. The notice rides the
    /// user-message stream and this mark rides beside it, so a reader does not
    /// mistake the notice for a message that opens a turn and split one user
    /// turn into two summary rows.
    #[serde(rename = "acpx/context/turn_interrupted")]
    ContextTurnInterrupted,

    // acpx/a2a/*, acpx/trajectory/*, acpx/cas/* — placeholder namespaces;
    // payload shapes land with their respective subsystems.
    #[serde(rename = "acpx/a2a/handoff")]
    A2aHandoff,
    #[serde(rename = "acpx/trajectory/snapshot")]
    TrajectorySnapshot,
    #[serde(rename = "acpx/cas/block_ref")]
    CasBlockRef,

    // acpx/session/takeControl — a session-scoped ext_method request (not a
    // notification): the client asks to take the control lease for a session,
    // optionally forcing (cancel the live turn + take the lease). The adapter
    // resolves the pending reverse request future and replies with a
    // TakeControlOutcome. Rides the ext_method request axis (carries a
    // req_id), unlike the notification methods above which have no req_id.
    #[serde(rename = "acpx/session/takeControl")]
    SessionTakeControl,

    /// A method this build does not know: a newer peer's addition, or a name
    /// this build writes wrong (a rename that drifted from its serde name).
    /// Either way it arrives here rather than failing the read that carried
    /// it. The name is not retained; a consumer reporting what arrived reads
    /// the params. This variant serializes as the literal Unknown, which no
    /// peer defines as a method.
    #[serde(other)]
    Unknown,
}

/// One acpx extension notification. The shape matches a base-protocol
/// ext_* notification: a method string (typed here) plus a params object the
/// method dictates. The adapter wraps this in the base notification envelope;
/// here it is the typed payload only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpxNotification {
    pub method: AcpxMethod,
    /// The method's own fields. JSON-RPC 2.0 lets a notification omit params,
    /// so an absent one reads as null here: the method still decodes, and a
    /// reader that looks a field up gets null rather than a failed read.
    #[serde(default)]
    pub params: Value,
}

impl AcpxNotification {
    pub fn new(method: AcpxMethod, params: Value) -> Self {
        Self { method, params }
    }
}

/// The typed capability block the adapter places at initialize-response
/// _meta.acpx. A pure-ACP client ignores _meta (unknown field); an
/// acpx-aware client reads it to learn streaming/cas/detach support and the
/// ext_method verbs the agent answers. Declaring capabilities here (not via
/// an ext_method probe) is atomic with the handshake, so a reconnecting
/// client knows lease/detach support before loadSession resends a pending
/// ask — there is no window between handshake and a probe reply.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpxCapabilities {
    /// True when the agent streams token-level acpx/llm/* notifications (the
    /// live-preview path). A pure-ACP client sees session/update only.
    #[serde(default)]
    pub streaming: bool,
    /// True when the agent supports content-addressed storage block
    /// retrieval (acpx/cas/*).
    #[serde(default)]
    pub cas: bool,
    /// True when the agent supports session detach + reattach (the
    /// control-lease lifecycle: loadSession + takeControl).
    #[serde(default)]
    pub detach: bool,
    /// The acpx/session/* ext_method request verbs the agent answers. A
    /// client probing an unlisted verb gets method-not-found, so this list
    /// is the probe-free discovery surface.
    #[serde(default)]
    pub ext_methods: Vec<String>,
}

/// Params for the acpx/session/takeControl ext_method request. session_id
/// identifies the session whose lease the caller wants; force = cancel the
/// live turn (the pending permission resolves to Cancelled) and take the
/// lease; without force the request waits for the pending prompt to finish.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TakeControlParams {
    pub session_id: String,
    #[serde(default)]
    pub force: bool,
}

/// The outcome the adapter returns for acpx/session/takeControl. Granted
/// carries pending_resent so the new holder knows whether a pending
/// permission ask was re-sent to it (the client surfaces the card). Denied
/// carries a reason (e.g. force unavailable, no such session). When force is
/// set, the adapter resolves to Granted only after the turn is cancelled and
/// the pending ask reaped — never while the old turn is live.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TakeControlOutcome {
    Granted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_resent: Option<bool>,
    },
    Denied {
        reason: String,
    },
}

/// Project a token-level LlmEvent onto its acpx/llm/* notification. The
/// method is keyed off the variant; the params carry the event's own fields
/// (serialized as the event's serde shape) so a client reconstructs the same
/// typed event. A future variant with no method mapping returns None so the
/// adapter drops it rather than inventing a key.
pub fn project_llm_event(event: &LlmEvent) -> Option<AcpxNotification> {
    let method = match event {
        LlmEvent::StepStart { .. } => AcpxMethod::LlmStepStart,
        LlmEvent::StepFinish { .. } => AcpxMethod::LlmStepFinish,
        LlmEvent::TextStart { .. } => AcpxMethod::LlmTextStart,
        LlmEvent::TextDelta { .. } => AcpxMethod::LlmTextDelta,
        LlmEvent::TextEnd { .. } => AcpxMethod::LlmTextEnd,
        LlmEvent::ReasoningStart { .. } => AcpxMethod::LlmReasoningStart,
        LlmEvent::ReasoningDelta { .. } => AcpxMethod::LlmReasoningDelta,
        LlmEvent::ReasoningEnd { .. } => AcpxMethod::LlmReasoningEnd,
        LlmEvent::ToolInputStart { .. } => AcpxMethod::LlmToolInputStart,
        LlmEvent::ToolInputDelta { .. } => AcpxMethod::LlmToolInputDelta,
        LlmEvent::ToolInputEnd { .. } => AcpxMethod::LlmToolInputEnd,
        LlmEvent::ToolCall { .. } => AcpxMethod::LlmToolCall,
        LlmEvent::ToolResult { .. } => AcpxMethod::LlmToolResult,
        LlmEvent::ToolError { .. } => AcpxMethod::LlmToolError,
        LlmEvent::Finish { .. } => AcpxMethod::LlmFinish,
        LlmEvent::ProviderError { .. } => AcpxMethod::LlmProviderError,
    };
    let params = serde_json::to_value(event).ok()?;
    Some(AcpxNotification::new(method, params))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Usage;

    #[test]
    fn test_method_serializes_slash_key() {
        // The wire key is the slash-namespaced string the base ext_*
        // mechanism carries, not a Rust-style snake name.
        let json = serde_json::to_string(&AcpxMethod::LlmTextDelta).unwrap();
        assert_eq!(json, r#""acpx/llm/text_delta""#);
        let back: AcpxMethod = serde_json::from_str(&json).unwrap();
        assert_eq!(back, AcpxMethod::LlmTextDelta);
    }

    #[test]
    fn test_take_control_method_serializes() {
        let json = serde_json::to_string(&AcpxMethod::SessionTakeControl).unwrap();
        assert_eq!(json, r#""acpx/session/takeControl""#);
        let back: AcpxMethod = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AcpxMethod::SessionTakeControl));
    }

    #[test]
    fn test_unknown_method_decodes() {
        // A method a newer peer added must not fail the read: the namespace
        // grows, and a build that cannot name a method still reads the
        // notification carrying it. The payload survives for a consumer to
        // report.
        let json = r#"{"method":"acpx/context/future_note","params":{"secs":12}}"#;
        let back: AcpxNotification = serde_json::from_str(json).unwrap();
        assert!(matches!(back.method, AcpxMethod::Unknown));
        assert_eq!(back.params["secs"], 12);
    }

    /// The name each variant carries on the wire. The match is wildcard-free
    /// on purpose: a variant added to the enum fails this build until its
    /// name is written here, because after the decode tolerance a name that
    /// is missing or spelled wrong turns that whole method category into a
    /// silent drop rather than an error. Unknown is pinned to the literal
    /// serde gives it, which no peer defines as a method.
    fn pinned_name(method: &AcpxMethod) -> &'static str {
        match method {
            AcpxMethod::LlmStepStart => "acpx/llm/step_start",
            AcpxMethod::LlmStepFinish => "acpx/llm/step_finish",
            AcpxMethod::LlmTextStart => "acpx/llm/text_start",
            AcpxMethod::LlmTextDelta => "acpx/llm/text_delta",
            AcpxMethod::LlmTextEnd => "acpx/llm/text_end",
            AcpxMethod::LlmReasoningStart => "acpx/llm/reasoning_start",
            AcpxMethod::LlmReasoningDelta => "acpx/llm/reasoning_delta",
            AcpxMethod::LlmReasoningEnd => "acpx/llm/reasoning_end",
            AcpxMethod::LlmToolInputStart => "acpx/llm/tool_input_start",
            AcpxMethod::LlmToolInputDelta => "acpx/llm/tool_input_delta",
            AcpxMethod::LlmToolInputEnd => "acpx/llm/tool_input_end",
            AcpxMethod::LlmToolCall => "acpx/llm/tool_call",
            AcpxMethod::LlmToolResult => "acpx/llm/tool_result",
            AcpxMethod::LlmToolError => "acpx/llm/tool_error",
            AcpxMethod::LlmFinish => "acpx/llm/finish",
            AcpxMethod::LlmProviderError => "acpx/llm/provider_error",
            AcpxMethod::MaxTurns => "acpx/max_turns",
            AcpxMethod::ToolProgress => "acpx/tool/progress",
            AcpxMethod::ContextCompactionBoundary => "acpx/context/compaction_boundary",
            AcpxMethod::ContextSummary => "acpx/context/summary",
            AcpxMethod::ContextMetaUser => "acpx/context/meta_user",
            AcpxMethod::ContextPermissionDecision => "acpx/context/permission_decision",
            AcpxMethod::ContextRunCompleted => "acpx/context/run_completed",
            AcpxMethod::ContextMidTurnInput => "acpx/context/mid_turn_input",
            AcpxMethod::ContextChildCompleted => "acpx/context/child_completed",
            AcpxMethod::ContextTurnInterrupted => "acpx/context/turn_interrupted",
            AcpxMethod::A2aHandoff => "acpx/a2a/handoff",
            AcpxMethod::TrajectorySnapshot => "acpx/trajectory/snapshot",
            AcpxMethod::CasBlockRef => "acpx/cas/block_ref",
            AcpxMethod::SessionTakeControl => "acpx/session/takeControl",
            AcpxMethod::Unknown => "Unknown",
        }
    }

    #[test]
    fn test_method_names_round_trip() {
        // Every named method is pinned both ways. A rename written wrong on
        // one side makes this build emit a name it cannot read back, and the
        // decode tolerance turns that into a silent drop, so this test is
        // what keeps a wrong name loud.
        let named = [
            AcpxMethod::LlmStepStart,
            AcpxMethod::LlmStepFinish,
            AcpxMethod::LlmTextStart,
            AcpxMethod::LlmTextDelta,
            AcpxMethod::LlmTextEnd,
            AcpxMethod::LlmReasoningStart,
            AcpxMethod::LlmReasoningDelta,
            AcpxMethod::LlmReasoningEnd,
            AcpxMethod::LlmToolInputStart,
            AcpxMethod::LlmToolInputDelta,
            AcpxMethod::LlmToolInputEnd,
            AcpxMethod::LlmToolCall,
            AcpxMethod::LlmToolResult,
            AcpxMethod::LlmToolError,
            AcpxMethod::LlmFinish,
            AcpxMethod::LlmProviderError,
            AcpxMethod::MaxTurns,
            AcpxMethod::ToolProgress,
            AcpxMethod::ContextCompactionBoundary,
            AcpxMethod::ContextSummary,
            AcpxMethod::ContextMetaUser,
            AcpxMethod::ContextPermissionDecision,
            AcpxMethod::ContextRunCompleted,
            AcpxMethod::ContextMidTurnInput,
            AcpxMethod::ContextChildCompleted,
            AcpxMethod::ContextTurnInterrupted,
            AcpxMethod::A2aHandoff,
            AcpxMethod::TrajectorySnapshot,
            AcpxMethod::CasBlockRef,
            AcpxMethod::SessionTakeControl,
        ];
        let mut seen = Vec::new();
        for method in named {
            let name = pinned_name(&method);
            let encoded = serde_json::to_string(&method).unwrap();
            assert_eq!(encoded, format!("\"{name}\""), "serialize {method:?}");
            let back: AcpxMethod = serde_json::from_str(&encoded).unwrap();
            assert_eq!(back, method, "read back {encoded}");
            seen.push(name);
        }
        // Two variants sharing a name would make one of them unreachable on
        // the wire while this test still passed row by row.
        let mut distinct = seen.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), seen.len(), "names are distinct: {seen:?}");
    }

    #[test]
    fn test_notification_without_params_decodes() {
        // JSON-RPC 2.0 lets a notification omit params; the method still
        // decodes and the fields read as null, so a peer that sends the bare
        // method does not end the read.
        let json = r#"{"method":"acpx/context/summary"}"#;
        let back: AcpxNotification = serde_json::from_str(json).unwrap();
        assert_eq!(back.method, AcpxMethod::ContextSummary);
        assert!(back.params.get("text").is_none());
    }

    #[test]
    fn test_capabilities_round_trip() {
        let caps = AcpxCapabilities {
            streaming: true,
            cas: false,
            detach: true,
            ext_methods: vec!["acpx/session/takeControl".into()],
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(json.contains(r#""streaming":true"#), "{json}");
        assert!(json.contains(r#""detach":true"#), "{json}");
        assert!(
            json.contains(r#""extMethods":["acpx/session/takeControl"]"#),
            "{json}"
        );
        let back: AcpxCapabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ext_methods, caps.ext_methods);
    }

    #[test]
    fn test_control_requires_session_id() {
        // {} without sessionId must fail — takeControl is session-scoped, the
        // target session is not optional.
        assert!(
            serde_json::from_str::<TakeControlParams>("{}").is_err(),
            "session_id must be required"
        );
        let p: TakeControlParams =
            serde_json::from_str(r#"{"sessionId":"01H","force":true}"#).unwrap();
        assert_eq!(p.session_id, "01H");
        assert!(p.force, "force decodes true");
        // force still defaults to false when only sessionId is present.
        let p2: TakeControlParams = serde_json::from_str(r#"{"sessionId":"01H"}"#).unwrap();
        assert!(!p2.force, "force defaults to false");
    }

    #[test]
    fn test_take_control_round_trips() {
        let granted = TakeControlOutcome::Granted {
            pending_resent: Some(true),
        };
        let j = serde_json::to_string(&granted).unwrap();
        assert_eq!(j, r#"{"type":"granted","pending_resent":true}"#);
        let denied = TakeControlOutcome::Denied {
            reason: "no session".into(),
        };
        let j2 = serde_json::to_string(&denied).unwrap();
        assert_eq!(j2, r#"{"type":"denied","reason":"no session"}"#);
        let back: TakeControlOutcome = serde_json::from_str(&j).unwrap();
        assert!(matches!(
            back,
            TakeControlOutcome::Granted {
                pending_resent: Some(true)
            }
        ));
    }

    #[test]
    fn test_notification_round_trips() {
        let n = AcpxNotification::new(
            AcpxMethod::LlmFinish,
            serde_json::json!({"reason": "stop", "usage": Usage::default()}),
        );
        let json = serde_json::to_string(&n).unwrap();
        let back: AcpxNotification = serde_json::from_str(&json).unwrap();
        assert_eq!(back.method, AcpxMethod::LlmFinish);
        assert!(back.params.get("reason").is_some());
    }

    #[test]
    fn test_text_delta_maps_params() {
        let ev = LlmEvent::TextDelta {
            id: "t1".into(),
            text: "hi".into(),
        };
        let n = project_llm_event(&ev).expect("text delta projects");
        assert_eq!(n.method, AcpxMethod::LlmTextDelta);
        assert_eq!(n.params["id"], "t1");
        assert_eq!(n.params["text"], "hi");
    }

    #[test]
    fn test_project_llm_event_variant() {
        // Every LlmEvent variant must map to a method (no silent drop).
        let cases: Vec<LlmEvent> = vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::StepFinish {
                index: 0,
                reason: "stop".into(),
                usage: None,
            },
            LlmEvent::TextStart { id: "t".into() },
            LlmEvent::TextDelta {
                id: "t".into(),
                text: "x".into(),
            },
            LlmEvent::TextEnd { id: "t".into() },
            LlmEvent::ReasoningStart { id: "r".into() },
            LlmEvent::ReasoningDelta {
                id: "r".into(),
                text: "x".into(),
            },
            LlmEvent::ReasoningEnd { id: "r".into() },
            LlmEvent::ToolInputStart {
                id: "ti".into(),
                name: "bash".into(),
            },
            LlmEvent::ToolInputDelta {
                id: "ti".into(),
                name: "bash".into(),
                text: "x".into(),
            },
            LlmEvent::ToolInputEnd {
                id: "ti".into(),
                name: "bash".into(),
            },
            LlmEvent::ToolCall {
                id: "c".into(),
                name: "bash".into(),
                input: serde_json::Value::Null,
            },
            LlmEvent::ToolResult {
                id: "c".into(),
                name: "bash".into(),
                output: serde_json::Value::Null,
            },
            LlmEvent::ToolError {
                id: "c".into(),
                name: "bash".into(),
                message: "boom".into(),
            },
            LlmEvent::Finish {
                reason: "stop".into(),
                usage: None,
            },
            LlmEvent::ProviderError {
                message: "x".into(),
                retryable: None,
            },
        ];
        for ev in &cases {
            assert!(project_llm_event(ev).is_some(), "variant must project");
        }
    }
}
