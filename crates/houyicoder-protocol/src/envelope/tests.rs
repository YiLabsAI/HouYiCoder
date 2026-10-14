//! Tests for this module.

use super::*;
use crate::frontend::FrontendRequest;
use crate::frontend::model::{
    AppliedModel, ContextWindow, ContextWindowSource, EffectiveFrom, EffortCapability,
    FastModeAvailability, ModelCatalog, ModelCatalogEntry, ModelChoice, ModelDisplayCapabilities,
    PersistenceOutcome, ResolvedModel, SpeedMode,
};
use crate::frontend::run::{ApprovalDecision, ApprovalRequest};
use crate::frontend::trust::TrustAccept;
use crate::frontend::trust::TrustPrompt;
use crate::frontend::trust::TrustRisk;
use crate::llm::EffortLevel;
use serde_json::Value;

fn sample_ask() -> ApprovalRequest {
    ApprovalRequest {
        call_id: "toolu_1".into(),
        tool_name: "bash".into(),
        input: Value::Null,
        options: Vec::new(),
        reason: None,
        delegation: None,
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
    let e = RequestEnvelope::new(RequestId(7), FrontendRequest::Console);
    let json = serde_json::to_string(&e).expect("serialize");
    let back: RequestEnvelope = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.req_id, RequestId(7));
}

#[test]
fn test_event_seq_orders_monotonically() {
    assert!(EventSeq(1) < EventSeq(2));
    assert!(EventSeq(2) > EventSeq(1));
}

fn sample_id() -> DurableEventId {
    DurableEventId("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string())
}

#[test]
fn test_envelope_carries_identity() {
    // The identity survives a round-trip and is visible in the
    // serialized form, so a consumer can stamp row identity from it.
    let envelope = EventEnvelope::durable(
        EventSeq(3),
        FrontendEvent::SystemLine {
            text: "delivered".into(),
        },
        sample_id(),
    );
    let json = serde_json::to_string(&envelope).expect("serialize");
    assert!(
        json.contains("\"event_id\""),
        "a durable envelope serializes its identity: {json}"
    );
    let back: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.event_id, Some(sample_id()));
    assert_eq!(back.seq, EventSeq(3));
}

#[test]
fn test_envelope_omits_identity() {
    // A runtime push with no log entry serializes without the key, and
    // the identity-free form — what a host that predates the field
    // sends — decodes to None rather than failing.
    let envelope = EventEnvelope::new(
        EventSeq(1),
        FrontendEvent::SystemLine {
            text: "runtime".into(),
        },
    );
    let json = serde_json::to_string(&envelope).expect("serialize");
    assert!(
        !json.contains("event_id"),
        "an absent identity leaves no key: {json}"
    );
    let back: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.event_id, None);
}

#[test]
fn test_legacy_consumer_ignores_identity() {
    // New host, old frontend: the envelope carries no deny-unknown
    // configuration, so a consumer whose shape predates the field
    // decodes the new form and every field it knows still lands.
    #[derive(Deserialize)]
    struct LegacyEnvelope {
        seq: EventSeq,
        payload: FrontendEvent,
    }
    let envelope = EventEnvelope::durable(
        EventSeq(7),
        FrontendEvent::SystemLine {
            text: "line".into(),
        },
        sample_id(),
    );
    let json = serde_json::to_string(&envelope).expect("serialize");
    let legacy: LegacyEnvelope =
        serde_json::from_str(&json).expect("a peer that predates the field ignores it");
    assert_eq!(legacy.seq, EventSeq(7));
    assert!(matches!(legacy.payload, FrontendEvent::SystemLine { .. }));
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
        ServerRequestPayload::TrustPrompt(TrustPrompt {
            project_path: "/proj".into(),
            risks: vec![TrustRisk {
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
        ClientResponsePayload::TrustAccept(TrustAccept { accepted: true }),
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
    let req = ClientFrame::Request(RequestEnvelope::new(RequestId(1), FrontendRequest::Console));
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
    // effort, speed carries the Fast tier (None = leave it untouched).
    // Every field survives a wire round-trip, verified by re-serializing
    // the deserialized envelope and comparing byte for byte.
    let req = RequestEnvelope::new(
        RequestId(9),
        FrontendRequest::ModelSet {
            model: None,
            effort: Some(EffortLevel::High),
            effort_toggled: true,
            speed: Some(SpeedMode::Fast),
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
    assert!(json.contains(r#""speed":"fast""#), "speed tier: {json}");
    let back: RequestEnvelope = serde_json::from_str(&json).expect("deserialize");
    let json2 = serde_json::to_string(&back).expect("reserialize");
    assert_eq!(json, json2, "round-trip preserves every field");
}

#[test]
fn test_model_set_speed_optional() {
    // A host that predates Fast mode sends no speed field; the request
    // still decodes with speed None, which leaves the tier untouched.
    let legacy = r#"{"req_id":9,"payload":{"ModelSet":{"model":"glm-5.2","effort":null,"effort_toggled":false}}}"#;
    let back: RequestEnvelope = serde_json::from_str(legacy).expect("deserialize");
    match back.payload {
        FrontendRequest::ModelSet { speed, .. } => {
            assert!(speed.is_none(), "absent speed leaves the tier untouched");
        }
        other => panic!("expected ModelSet, got {other:?}"),
    }
}

#[test]
fn test_model_result_roundtrips() {
    // The reply: the host resolves the sentinel to a real id and reports
    // the effort it will actually send. effort None is honest when the
    // host is not sending an effort parameter.
    let reply = ResponseEnvelope::new(
        RequestId(9),
        ResponsePayload::ModelResult(ModelApplyResult {
            selected: ModelChoice::Default,
            applied: AppliedModel {
                id: "qwen3.7-max".into(),
                effort: None,
                speed: SpeedMode::Standard,
            },
            effective_from: EffectiveFrom::Immediate,
            persistence: PersistenceOutcome::Saved,
        }),
    );
    let json = serde_json::to_string(&reply).expect("serialize");
    let back: ResponseEnvelope = serde_json::from_str(&json).expect("deserialize");
    let json2 = serde_json::to_string(&back).expect("reserialize");
    assert_eq!(json, json2, "ModelApplyResult round-trips byte for byte");
    assert!(json.contains(r#""type":"model_result""#));
    assert!(json.contains(r#""effort":null"#));
    assert!(json.contains("qwen3.7-max"));
}

#[test]
fn test_model_result_defaults_apply() {
    // Optional facts (effort, speed) default rather than failing the
    // decode; persistence does not — a payload without it fails closed
    // instead of reading an unknown outcome as Saved.
    let legacy = r#"{"req_id":9,"payload":{"type":"model_result","data":{"selected":{"mode":"default"},"applied":{"id":"glm-5.2"},"effective_from":"immediate"}}}"#;
    let err = serde_json::from_str::<ResponseEnvelope>(legacy)
        .expect_err("a payload without persistence must not decode");
    assert!(
        err.to_string().contains("persistence"),
        "the decode names the missing field: {err}"
    );
    let with_persistence = r#"{"req_id":9,"payload":{"type":"model_result","data":{"selected":{"mode":"default"},"applied":{"id":"glm-5.2"},"effective_from":"immediate","persistence":{"outcome":"saved"}}}}"#;
    let back: ResponseEnvelope = serde_json::from_str(with_persistence).expect("deserialize");
    match back.payload {
        ResponsePayload::ModelResult(result) => {
            assert_eq!(result.applied.id, "glm-5.2");
            assert_eq!(result.applied.effort, None, "effort defaults to auto");
            assert_eq!(result.applied.speed, SpeedMode::Standard);
            assert_eq!(result.persistence, PersistenceOutcome::Saved);
        }
        other => panic!("expected ModelResult, got {other:?}"),
    }
}

#[test]
fn test_model_info_catalog_roundtrips() {
    // The /model pane catalog snapshot survives a wire round-trip: the
    // selection, applied model, Default resolution, effort fallback and
    // catalog entries (with display_name + description + effort +
    // capabilities) all preserve.
    let reply = ResponseEnvelope::new(
        RequestId(11),
        ResponsePayload::ModelInfo(ModelCatalog {
            selected: ModelChoice::Explicit {
                id: "qwen3.7-max".into(),
            },
            applied: AppliedModel {
                id: "qwen3.7-max".into(),
                effort: Some(EffortLevel::High),
                speed: SpeedMode::Fast,
            },
            resolved_default: ResolvedModel {
                id: "qwen3.7-max".into(),
                capabilities: Default::default(),
            },
            effort_level: Some(EffortLevel::High),
            entries: vec![ModelCatalogEntry {
                id: "qwen3.7-max".into(),
                display_name: Some("Max".into()),
                description: Some("most capable".into()),
                effort: Some(EffortLevel::Medium),
                capabilities: ModelDisplayCapabilities {
                    context_window: Some(ContextWindow {
                        tokens: 1_000_000,
                        source: ContextWindowSource::ModelSuffix,
                    }),
                    max_output_tokens: Some(32_768),
                    effort: EffortCapability::Supported {
                        levels: vec![EffortLevel::Low, EffortLevel::Medium, EffortLevel::High],
                    },
                    fast: FastModeAvailability::Available,
                },
            }],
        }),
    );
    let json = serde_json::to_string(&reply).expect("serialize");
    let back: ResponseEnvelope = serde_json::from_str(&json).expect("deserialize");
    match back.payload {
        ResponsePayload::ModelInfo(catalog) => {
            assert_eq!(
                catalog.selected,
                ModelChoice::Explicit {
                    id: "qwen3.7-max".into()
                }
            );
            assert_eq!(catalog.applied.id, "qwen3.7-max");
            assert_eq!(catalog.applied.speed, SpeedMode::Fast);
            assert_eq!(catalog.resolved_default.id, "qwen3.7-max");
            assert_eq!(catalog.effort_level, Some(EffortLevel::High));
            assert_eq!(catalog.entries.len(), 1);
            assert_eq!(catalog.entries[0].display_name.as_deref(), Some("Max"));
            assert_eq!(catalog.entries[0].effort, Some(EffortLevel::Medium));
            assert_eq!(
                catalog.entries[0].capabilities.context_window,
                Some(ContextWindow {
                    tokens: 1_000_000,
                    source: ContextWindowSource::ModelSuffix,
                })
            );
            assert_eq!(
                catalog.entries[0].capabilities.max_output_tokens,
                Some(32_768)
            );
        }
        other => panic!("expected ModelInfo, got {other:?}"),
    }
}
