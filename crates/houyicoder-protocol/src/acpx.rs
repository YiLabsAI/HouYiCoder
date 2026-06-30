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
//! matches the typed enum, and a typo or a new method surfaces at compile
//! time. The adapter (service layer) maps the wire string to the enum on the
//! way in and back on the way out.
//!
//! LlmEvent (token-level provider stream) projects onto acpx/llm/* as an
//! independent notification stream — it does NOT ride the base session/update
//! channel, which carries the turn-level TurnEvent projection. The two are
//! orthogonal: LlmEvent is live token flow; TurnEvent is the durable turn
//! record.

use crate::llm::LlmEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The typed acpx/* method namespace. Wire-serializes to the string key the
/// base protocol ext_* mechanism carries (e.g. LlmTextDelta serializes as
/// "acpx/llm/text_delta"). non_exhaustive so a new extension method lands
/// without reworking every match; unknown methods decode to None at the
/// adapter boundary (a standard client ignores them).
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

    // acpx/context/* — TurnEvent kinds the base session/update has no
    // standard counterpart for (CompactionBoundary, Summary, MetaUser,
    // PermissionDecision). These ride the extension notification stream
    // (the durable-context audit trail), orthogonal to session/update.
    #[serde(rename = "acpx/context/compaction_boundary")]
    ContextCompactionBoundary,
    #[serde(rename = "acpx/context/summary")]
    ContextSummary,
    #[serde(rename = "acpx/context/meta_user")]
    ContextMetaUser,
    #[serde(rename = "acpx/context/permission_decision")]
    ContextPermissionDecision,

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
}

/// One acpx extension notification. The shape matches a base-protocol
/// ext_* notification: a method string (typed here) plus a params object the
/// method dictates. The adapter wraps this in the base notification envelope;
/// here it is the typed payload only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpxNotification {
    pub method: AcpxMethod,
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
