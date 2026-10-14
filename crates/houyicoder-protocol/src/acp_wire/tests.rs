//! Tests for this module.

use super::*;
use crate::acpx::AcpxCapabilities;
use crate::frontend::run::StopReason;
use serde_json::Map;

#[test]
fn test_request_round_trips() {
    let req = AcpRequest::new(7, "initialize", serde_json::json!({"protocolVersion": 1}));
    let j = serde_json::to_string(&req).unwrap();
    assert_eq!(
        j,
        r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"protocolVersion":1}}"#
    );
    let back: AcpRequest = serde_json::from_str(&j).unwrap();
    assert_eq!(back.method, "initialize");
    assert!(matches!(back.id, AcpRequestId::Number(7)));
}

#[test]
fn test_response_distinguishes_result_error() {
    let ok = AcpResponse::ok(
        AcpRequestId::Number(3),
        serde_json::json!({"stopReason":"end_turn"}),
    );
    let j = serde_json::to_string(&ok).unwrap();
    assert!(j.contains(r#""result":{"stopReason":"end_turn"}"#), "{j}");
    assert!(!j.contains("error"), "{j}");
    let back: AcpResponse = serde_json::from_str(&j).unwrap();
    assert!(matches!(back, AcpResponse::Result { .. }));

    let err = AcpResponse::err(
        AcpRequestId::Str("x".into()),
        AcpErrorCode::MethodNotFound,
        "no such",
    );
    let j2 = serde_json::to_string(&err).unwrap();
    assert!(j2.contains(r#""code":-32601"#), "{j2}");
    assert!(j2.contains(r#""message":"no such""#), "{j2}");
    let back2: AcpResponse = serde_json::from_str(&j2).unwrap();
    assert!(matches!(back2, AcpResponse::Error { .. }));
}

#[test]
fn test_notification_has_no_id() {
    let n = AcpNotification::new("session/update", serde_json::json!({"x": 1}));
    let j = serde_json::to_string(&n).unwrap();
    assert_eq!(
        j,
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"x":1}}"#
    );
    // A notification must not carry an id field.
    assert!(!j.contains(r#""id""#), "notification leaked an id: {j}");
}

#[test]
fn test_request_id_accepts_kinds() {
    for (json, expected) in [
        (r#"null"#, AcpRequestId::Null),
        (r#"42"#, AcpRequestId::Number(42)),
        (r#""abc""#, AcpRequestId::Str("abc".into())),
    ] {
        let id: AcpRequestId = serde_json::from_str(json).unwrap();
        assert_eq!(id, expected);
    }
}

#[test]
fn test_error_code_round_trips() {
    for code in [
        AcpErrorCode::ParseError,
        AcpErrorCode::MethodNotFound,
        AcpErrorCode::AuthRequired,
        AcpErrorCode::Other(-9999),
    ] {
        let v = serde_json::to_value(code).unwrap();
        assert!(v.is_number(), "{code:?} must serialize as a number");
        let back: AcpErrorCode = serde_json::from_value(v).unwrap();
        assert_eq!(back, code);
    }
}

#[test]
fn test_initialize_response_carries_meta() {
    let caps = AcpxCapabilities {
        streaming: true,
        cas: false,
        detach: true,
        ext_methods: vec!["acpx/session/takeControl".into()],
    };
    let mut meta = Map::new();
    meta.insert("acpx".into(), serde_json::to_value(&caps).unwrap());
    let resp = InitializeResponse {
        protocol_version: 1,
        agent_capabilities: AgentCapabilities {
            load_session: true,
            ..Default::default()
        },
        auth_methods: Vec::new(),
        agent_info: Some(Implementation {
            name: "agent".into(),
            version: "0.1".into(),
            ..Default::default()
        }),
        meta: Some(meta),
    };
    let j = serde_json::to_string(&resp).unwrap();
    assert!(j.contains(r#""protocolVersion":1"#), "{j}");
    assert!(j.contains(r#""loadSession":true"#), "{j}");
    assert!(j.contains(r#""_meta":{"acpx":{"#), "{j}");
    assert!(j.contains(r#""streaming":true"#), "{j}");
    assert!(
        j.contains(r#""extMethods":["acpx/session/takeControl"]"#),
        "{j}"
    );
    let back: InitializeResponse = serde_json::from_str(&j).unwrap();
    assert_eq!(back.protocol_version, 1);
    assert!(back.agent_capabilities.load_session);
    // The _meta.acpx block round-trips as raw JSON; the adapter decodes it
    // back to AcpxCapabilities on the client side.
    let back_caps: AcpxCapabilities =
        serde_json::from_value(back.meta.unwrap().get("acpx").unwrap().clone()).unwrap();
    assert_eq!(back_caps.ext_methods, caps.ext_methods);
}

#[test]
fn test_prompt_response_round_trips() {
    let resp = PromptResponse {
        stop_reason: StopReason::EndTurn,
        meta: None,
    };
    let j = serde_json::to_string(&resp).unwrap();
    assert_eq!(j, r#"{"stopReason":"end_turn"}"#);
    let back: PromptResponse = serde_json::from_str(&j).unwrap();
    assert!(matches!(back.stop_reason, StopReason::EndTurn));
}

#[test]
fn test_permission_outcome_double_nesting() {
    let cancelled = RequestPermissionResponse {
        outcome: RequestPermissionOutcome::Cancelled,
        meta: None,
    };
    let j = serde_json::to_string(&cancelled).unwrap();
    assert_eq!(j, r#"{"outcome":{"outcome":"cancelled"}}"#);
    let back: RequestPermissionResponse = serde_json::from_str(&j).unwrap();
    assert!(matches!(back.outcome, RequestPermissionOutcome::Cancelled));

    let selected = RequestPermissionResponse {
        outcome: RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
            option_id: "allow_once".into(),
            meta: None,
        }),
        meta: None,
    };
    let j2 = serde_json::to_string(&selected).unwrap();
    assert_eq!(
        j2,
        r#"{"outcome":{"outcome":"selected","optionId":"allow_once"}}"#
    );
}

#[test]
fn test_ext_request_prefixes_method() {
    let r = AcpRequest::ext_request(
        1,
        "acpx/session/takeControl",
        serde_json::json!({"force": true}),
    );
    let j = serde_json::to_string(&r).unwrap();
    assert!(j.contains(r#""method":"_acpx/session/takeControl""#), "{j}");
    assert!(j.contains(r#""params":{"force":true}"#), "{j}");
}
