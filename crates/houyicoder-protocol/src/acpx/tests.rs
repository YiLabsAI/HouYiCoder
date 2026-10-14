//! Tests for this module.

use super::*;
use crate::llm::Usage;
use serde_json::Value;

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
    let p: TakeControlParams = serde_json::from_str(r#"{"sessionId":"01H","force":true}"#).unwrap();
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
    let n = map_llm_event(&ev).expect("text delta maps");
    assert_eq!(n.method, AcpxMethod::LlmTextDelta);
    assert_eq!(n.params["id"], "t1");
    assert_eq!(n.params["text"], "hi");
}

#[test]
fn test_llm_event_maps_variant() {
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
            input: Value::Null,
        },
        LlmEvent::ToolResult {
            id: "c".into(),
            name: "bash".into(),
            output: Value::Null,
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
        assert!(map_llm_event(ev).is_some(), "variant must map");
    }
}
