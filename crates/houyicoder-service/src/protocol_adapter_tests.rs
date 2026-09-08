use super::*;
use houyicoder_context::PermissionVerdict;

/// Every engine turn-event kind maps to exactly one stream — a
/// session/update variant or an acpx/context notification — except
/// streaming assistant deltas, which map to neither: a delta is the
/// live audit trail subsumed by the authoritative AssistantMessage at
/// turn end, so neither stream carries it (the live preview rides the
/// shared live sink, not the wire). A new kind that fails to map surfaces
/// here, not in production.
#[test]
fn test_every_kind_maps() {
    let cases: Vec<(SessionEvent, bool, bool)> = vec![
        (SessionEvent::UserInput { text: "hi".into() }, true, false),
        (
            SessionEvent::AssistantMessage {
                text: "yo".into(),
                thinking: None,
            },
            true,
            false,
        ),
        // Streaming deltas are transient: subsumed by the final
        // AssistantMessage, so the wire carries neither the delta nor
        // an acpx counterpart.
        (
            SessionEvent::AssistantTextDelta { text: "d".into() },
            false,
            false,
        ),
        (SessionEvent::Reasoning { text: "r".into() }, true, false),
        (
            SessionEvent::ToolCall {
                call_id: "c".into(),
                tool: "bash".into(),
                input: serde_json::Value::Null,
            },
            true,
            false,
        ),
        (
            SessionEvent::tool_result("c", serde_json::Value::Null),
            true,
            false,
        ),
        (
            SessionEvent::MetaUser {
                text: "nudge".into(),
            },
            false,
            true,
        ),
        (
            SessionEvent::CompactionBoundary {
                checkpoint: Default::default(),
            },
            false,
            true,
        ),
        (SessionEvent::Summary { text: "s".into() }, false, true),
        (
            SessionEvent::PermissionDecision {
                call_id: "c".into(),
                tool: "bash".into(),
                verdict: PermissionVerdict::Approved,
                scope: "once".into(),
            },
            false,
            true,
        ),
        // Unknown lands on neither stream: a future binary's event type the
        // current binary does not recognize carries no mapping.
        (SessionEvent::Unknown, false, false),
    ];
    for (kind, expects_update, expects_acpx) in cases {
        assert_eq!(
            map_session_update(&kind).is_some(),
            expects_update,
            "session/update mapping mismatch for {:?}",
            kind
        );
        assert_eq!(
            map_acpx_notification(&kind).is_some(),
            expects_acpx,
            "acpx mapping mismatch for {:?}",
            kind
        );
    }
}

#[test]
fn test_tool_result_maps_update() {
    let kind = SessionEvent::tool_result("toolu_1", serde_json::Value::String("ok".into()));
    let update = map_session_update(&kind).expect("tool result maps");
    let SessionUpdate::ToolCallUpdate(upd) = update else {
        panic!("tool result is a tool-call update");
    };
    assert_eq!(upd.tool_call_id.0, "toolu_1");
    assert_eq!(upd.fields.status, Some(ToolCallStatus::Completed));
    assert_eq!(
        upd.fields.raw_output,
        Some(serde_json::Value::String("ok".into()))
    );
}

#[test]
fn test_permission_decision_maps_acpx() {
    let kind = SessionEvent::PermissionDecision {
        call_id: "c".into(),
        tool: "bash".into(),
        verdict: PermissionVerdict::Denied,
        scope: "session".into(),
    };
    let n = map_acpx_notification(&kind).expect("verdict maps");
    assert_eq!(n.method, AcpxMethod::ContextPermissionDecision);
    assert_eq!(n.params["verdict"], "denied");
    assert_eq!(n.params["scope"], "session");
}

#[test]
fn test_approval_maps_acp_permission() {
    let req = houyicoder_core::agent::ApprovalRequest::new(
        "call_1".into(),
        "bash".into(),
        serde_json::json!({"cmd": "ls"}),
    );
    let ask = approval_to_acp_permission(&req, "01S".into());
    assert_eq!(ask.session_id, "01S");
    assert_eq!(ask.tool_call.tool_call_id.0, "call_1");
    assert_eq!(
        ask.tool_call.fields.raw_input,
        Some(serde_json::json!({"cmd": "ls"}))
    );
    assert_eq!(ask.options.len(), 4);
    let ids: Vec<_> = ask.options.iter().map(|o| o.option_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["allow_once", "allow_always", "reject_once", "reject_always"]
    );
}

#[test]
fn test_allow_approves_reject_denies() {
    let call_id = "c".to_string();
    for (option_id, approved) in [
        ("allow_once", true),
        ("allow_always", true),
        ("reject_once", false),
        ("reject_always", false),
    ] {
        let resp = RequestPermissionResponse {
            outcome: RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                option_id: option_id.into(),
                meta: None,
            }),
            meta: None,
        };
        let d = acp_permission_response_to_decision(resp, call_id.clone());
        assert_eq!(d.approved, approved, "option {option_id}");
        assert_eq!(d.call_id, "c");
        assert!(d.updated_input.is_none());
    }
}

#[test]
fn test_cancelled_permission_denies() {
    let resp = RequestPermissionResponse {
        outcome: RequestPermissionOutcome::Cancelled,
        meta: None,
    };
    let d = acp_permission_response_to_decision(resp, "c".into());
    assert!(!d.approved, "a cancelled ask must deny the tool");
}

#[test]
fn test_permission_mode_both_ways() {
    use houyicoder_permission::PermissionMode as E;
    // The two modes round-trip engine -> wire -> engine.
    assert!(matches!(
        permission_mode_from_wire(permission_mode_to_wire(E::Manual)),
        E::Manual
    ));
    assert!(matches!(
        permission_mode_from_wire(permission_mode_to_wire(E::Auto)),
        E::Auto
    ));
}

/// The rule's persistence scope (destination) round-trips engine -> wire
/// -> engine across all three scopes, so a rule the /permissions Add flow
/// lands in a chosen destination hydrates back from that same scope.
#[test]
fn test_rule_destination_round_trips() {
    use houyicoder_permission::{Effect, Rule, RuleContent, Scope};
    use houyicoder_protocol::frontend::permission::RuleDestination;
    for (scope, dest) in [
        (Scope::User, RuleDestination::User),
        (Scope::Project, RuleDestination::Project),
        (Scope::Local, RuleDestination::Local),
    ] {
        let rule = Rule::with_content("bash", RuleContent::Prefix("npm".into()), Effect::Allow)
            .unwrap()
            .with_scope(scope);
        let wire = permission_rule_to_wire(&rule);
        assert_eq!(wire.destination, dest, "scope {scope:?} -> wire");
        let back = permission_rule_from_wire(&wire).expect("wire -> engine");
        assert_eq!(back.scope, scope, "wire {dest:?} -> engine scope");
    }
}

/// TurnAborted maps to the "aborted" trajectory label, a visible
/// session-update message chunk (so the host renders the boundary
/// notice), and skips the acpx context mapping (it is not a
/// model-input or side-channel event).
#[test]
fn test_turn_aborted_maps_label() {
    use houyicoder_context::SessionEvent;
    let kind = SessionEvent::TurnAborted {
        reason: "crash".into(),
    };
    assert_eq!(event_name(&kind), "aborted");
    assert!(
        map_acpx_notification(&kind).is_none(),
        "acpx context skips TurnAborted"
    );
    // The session-update mapping must produce a visible message chunk
    // so the host renders the boundary notice (guardrail 3).
    let update = map_session_update(&kind);
    let s = serde_json::to_string(&update).unwrap_or_default();
    assert!(
        s.contains("previous turn was interrupted"),
        "session-update must carry the notice: {s}"
    );
}

/// An Auth error surfaces a message that names the API key, so the user
/// debugs credentials, not the model id.
#[test]
fn test_auth_error_mentions_key() {
    let e = houyicoder_core::agent::RunError::ProviderFatal(
        houyicoder_protocol::llm::ProviderError::Auth,
    );
    let wire = super::map_run_error(&e);
    assert!(
        wire.message.contains("API key"),
        "auth → key hint: {}",
        wire.message
    );
}

/// A ModelNotFound error surfaces a message that points at the catalog and
/// never mentions the API key (the "don't mislead" rule).
#[test]
fn test_not_found_omits_key() {
    let e = houyicoder_core::agent::RunError::ProviderFatal(
        houyicoder_protocol::llm::ProviderError::ModelNotFound("qwen3.8-max".into()),
    );
    let wire = super::map_run_error(&e);
    assert!(
        wire.message.contains("catalog"),
        "model-not-found → catalog hint: {}",
        wire.message
    );
    assert!(
        !wire.message.contains("API key"),
        "model-not-found must not mention key: {}",
        wire.message
    );
    assert!(
        wire.message.contains("qwen3.8-max"),
        "model-not-found names the id: {}",
        wire.message
    );
}

#[test]
fn test_build_trajectory_carries_duration() {
    use houyicoder_context::{EventId, SessionId, SessionLogEntry};
    let mk = |kind| SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: kind,
    };
    let tool = mk(SessionEvent::ToolResult {
        call_id: "c1".into(),
        output: serde_json::json!({}),
        duration_ms: 4200,
    });
    let entries = super::build_trajectory_entries(std::slice::from_ref(&tool));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].duration_ms, Some(4200));
    let user = mk(SessionEvent::UserInput { text: "hi".into() });
    let entries2 = super::build_trajectory_entries(std::slice::from_ref(&user));
    assert_eq!(entries2[0].duration_ms, None);
}

/// A skill-tool always-allow scopes to the specific skill name + lands at
/// Local (machine-local), not a blanket repo-shared tool-level rule. One
/// approval cannot pre-authorize every future skill invocation for
/// collaborators, and a different skill re-asks.
#[test]
fn test_skill_consent_name_local() {
    use houyicoder_permission::{Effect, RuleContent, Scope};
    let rule = super::consent_rule_for("skill", &serde_json::json!({"skill": "deploy"}))
        .expect("skill rule");
    assert_eq!(rule.action, "skill");
    assert_eq!(rule.effect, Effect::Allow);
    assert_eq!(
        rule.scope,
        Scope::Local,
        "lands machine-local, not repo-shared"
    );
    match &rule.content {
        Some(RuleContent::Exact(s)) => assert_eq!(s, "deploy", "scoped to the specific skill name"),
        _ => panic!("expected Exact(deploy), not a blanket tool rule"),
    }

    // A different skill name produces a different Exact rule (per-skill).
    let rule2 =
        super::consent_rule_for("skill", &serde_json::json!({"skill": "lint"})).expect("lint rule");
    match &rule2.content {
        Some(RuleContent::Exact(s)) => assert_eq!(s, "lint"),
        _ => panic!("expected Exact(lint)"),
    }

    // A missing skill name installs nothing durable (approved once only).
    assert!(
        super::consent_rule_for("skill", &serde_json::json!({})).is_none(),
        "missing skill name: no durable rule"
    );
}

/// The per-skill consent rule, once installed in the gate, matches ONLY the
/// approved skill and not a different one. This is the landmine-defusal pin:
/// if the skill tool ever joins the gate ladder, a persisted rule grants only
/// the skill the user actually approved, not every future skill invocation.
#[test]
fn test_skill_rule_scoped() {
    use houyicoder_permission::{DefaultModeGate, ModeGate, Outcome, ToolRequest};
    let gate = DefaultModeGate::new();
    let rule =
        super::consent_rule_for("skill", &serde_json::json!({"skill": "deploy"})).expect("rule");
    gate.add_rule(rule);

    // Same skill: the rule matches at RuleAllow -> Allow.
    let deploy = serde_json::json!({"skill": "deploy"});
    let req = ToolRequest {
        tool_name: "skill",
        input: Some(&deploy),
        is_destructive: false,
        is_read_only: false,
        native_requires_approval: true,
    };
    assert_eq!(
        gate.decide(&req).outcome(),
        Outcome::Allow,
        "same skill: the per-skill rule matches"
    );

    // Different skill: no match -> re-asks (not Allow).
    let lint = serde_json::json!({"skill": "lint"});
    let req2 = ToolRequest {
        tool_name: "skill",
        input: Some(&lint),
        is_destructive: false,
        is_read_only: false,
        native_requires_approval: true,
    };
    assert_ne!(
        gate.decide(&req2).outcome(),
        Outcome::Allow,
        "different skill: the per-skill rule does not blanket-match"
    );
}
