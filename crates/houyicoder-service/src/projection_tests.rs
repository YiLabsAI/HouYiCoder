use super::*;
use houyicoder_context::PermissionVerdict;

/// Every engine turn-event kind projects to exactly one stream — a
/// session/update variant or an acpx/context notification — except
/// streaming assistant deltas, which project to neither: a delta is the
/// live audit trail subsumed by the authoritative AssistantMessage at
/// turn end, so neither stream carries it (the live preview rides the
/// shared live sink, not the wire). A new kind that fails to map surfaces
/// here, not in production.
#[test]
fn test_every_kind_projects() {
    let cases: Vec<(TurnEventKind, bool, bool)> = vec![
        (TurnEventKind::UserInput { text: "hi".into() }, true, false),
        (
            TurnEventKind::AssistantMessage {
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
            TurnEventKind::AssistantTextDelta { text: "d".into() },
            false,
            false,
        ),
        (TurnEventKind::Reasoning { text: "r".into() }, true, false),
        (
            TurnEventKind::ToolCall {
                call_id: "c".into(),
                tool: "bash".into(),
                input: serde_json::Value::Null,
            },
            true,
            false,
        ),
        (
            TurnEventKind::tool_result("c", serde_json::Value::Null),
            true,
            false,
        ),
        (
            TurnEventKind::MetaUser {
                text: "nudge".into(),
            },
            false,
            true,
        ),
        (
            TurnEventKind::CompactionBoundary {
                checkpoint: Default::default(),
            },
            false,
            true,
        ),
        (TurnEventKind::Summary { text: "s".into() }, false, true),
        (
            TurnEventKind::PermissionDecision {
                call_id: "c".into(),
                tool: "bash".into(),
                verdict: PermissionVerdict::Approved,
                scope: "once".into(),
            },
            false,
            true,
        ),
        // Unknown lands on neither stream: a future binary's event type the
        // current binary does not recognize carries no projection.
        (TurnEventKind::Unknown, false, false),
    ];
    for (kind, expects_update, expects_acpx) in cases {
        assert_eq!(
            project_session_update(&kind).is_some(),
            expects_update,
            "session/update projection mismatch for {:?}",
            kind
        );
        assert_eq!(
            project_acpx_context(&kind).is_some(),
            expects_acpx,
            "acpx projection mismatch for {:?}",
            kind
        );
    }
}

#[test]
fn test_tool_result_projects_update() {
    let kind = TurnEventKind::tool_result("toolu_1", serde_json::Value::String("ok".into()));
    let update = project_session_update(&kind).expect("tool result projects");
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
fn test_permission_decision_projects_acpx() {
    let kind = TurnEventKind::PermissionDecision {
        call_id: "c".into(),
        tool: "bash".into(),
        verdict: PermissionVerdict::Denied,
        scope: "session".into(),
    };
    let n = project_acpx_context(&kind).expect("verdict projects");
    assert_eq!(n.method, AcpxMethod::ContextPermissionDecision);
    assert_eq!(n.params["verdict"], "denied");
    assert_eq!(n.params["scope"], "session");
}

#[test]
fn test_approval_projects_acp_permission() {
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
        wire_mode_to_engine(project_permission_mode(E::Manual)),
        E::Manual
    ));
    assert!(matches!(
        wire_mode_to_engine(project_permission_mode(E::Auto)),
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
        let wire = project_permission_rule(&rule);
        assert_eq!(wire.destination, dest, "scope {scope:?} -> wire");
        let back = wire_rule_to_engine(&wire).expect("wire -> engine");
        assert_eq!(back.scope, scope, "wire {dest:?} -> engine scope");
    }
}

/// TurnAborted projects to the "aborted" trajectory label, a visible
/// session-update message chunk (so the host renders the boundary
/// notice), and skips the acpx context projection (it is not a
/// model-input or side-channel event).
#[test]
fn test_turn_aborted_projects_label() {
    use houyicoder_context::TurnEventKind;
    let kind = TurnEventKind::TurnAborted {
        reason: "crash".into(),
    };
    assert_eq!(trajectory_kind_label(&kind), "aborted");
    assert!(
        project_acpx_context(&kind).is_none(),
        "acpx context skips TurnAborted"
    );
    // The session-update projection must produce a visible message chunk
    // so the host renders the boundary notice (guardrail 3).
    let update = project_session_update(&kind);
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
    let wire = super::project_run_error(&e);
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
    let wire = super::project_run_error(&e);
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
fn test_project_trajectory_carries_duration() {
    use houyicoder_context::{EventId, SessionId, TurnEvent};
    let mk = |kind| TurnEvent {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        kind,
    };
    let tool = mk(TurnEventKind::ToolResult {
        call_id: "c1".into(),
        output: serde_json::json!({}),
        duration_ms: 4200,
    });
    let entries = super::project_trajectory(std::slice::from_ref(&tool));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].duration_ms, Some(4200));
    let user = mk(TurnEventKind::UserInput { text: "hi".into() });
    let entries2 = super::project_trajectory(std::slice::from_ref(&user));
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
