use super::*;
use houyicoder_api::sandbox::{BoundaryAccess, BoundaryGrant};
use houyicoder_api::session::{ApprovalAudit, TurnAudit};
use houyicoder_context::PermissionVerdict;
use houyicoder_context::RecapSource;
use houyicoder_context::RecapUsage;
use houyicoder_context::SessionEvent;
use houyicoder_context::{EventId, SessionId, SessionLogEntry};
use houyicoder_core::agent::ApprovalRequest;
use houyicoder_core::agent::RunError;
use houyicoder_permission::PermissionMode as E;
use houyicoder_permission::{Decision, DefaultModeGate, ModeGate, Outcome, ToolRequest};
use houyicoder_permission::{Effect, Rule, RuleContent, Scope};
use houyicoder_protocol::acp_wire::{
    RequestPermissionOutcome, RequestPermissionResponse, SelectedPermissionOutcome,
};
use houyicoder_protocol::acpx::AcpxMethod;
use houyicoder_protocol::frontend::permission::{AskSource, RuleDestination};
use houyicoder_protocol::frontend::run::ApprovalScope;
use houyicoder_protocol::llm::ProviderError;
use serde_json::Value;
use std::path::PathBuf;
use std::slice::from_ref;

#[test]
fn test_file_output_once() {
    let gate = DefaultModeGate::with_mode(E::Auto);
    let input = serde_json::json!({
        "command": "curl https://example.test/ -o \"$TMPDIR/result.log\""
    });
    let request = ToolRequest {
        tool_name: "bash",
        input: Some(&input),
        is_destructive: true,
        is_read_only: false,
        native_requires_approval: true,
    };
    let reason = match gate.decide(&request) {
        Decision::Ask(reason) => AskReason::from(&reason),
        other => panic!("expected file output approval: {other:?}"),
    };
    assert!(matches!(
        durable_grant("bash", &input, Some(&reason), UNFENCED, &[]),
        DurableGrant::Once
    ));
    let options = approval_options("bash", &input, Some(&reason), UNFENCED, &[]);
    let ids: Vec<&str> = options.iter().map(|o| o.option_id.as_str()).collect();
    assert_eq!(ids, ["allow_once", "reject_once"]);
    gate.add_egress_allow("curl https://example.test");
    assert_eq!(gate.decide(&request).outcome(), Outcome::Ask);
}

/// A git op outside the four history-rewriting verbs keeps a single approval.
/// Only commit, rebase, reset, and tag mint a durable rule, so a discard form
/// must not promise a session-wide remember the store would never receive.
#[test]
fn test_git_discard_keeps_once() {
    let input = serde_json::json!({"command": "git checkout -- crates/foo.rs"});
    let reason = AskReason {
        source: AskSource::Detection,
        validator: "git_checkpoint".to_string(),
        detail: "a git discard form".into(),
        containment_note: None,
    };
    assert!(matches!(
        durable_grant("bash", &input, Some(&reason), FENCED, &[]),
        DurableGrant::Once
    ));
}

#[test]
fn test_read_scope_remember() {
    let gate = DefaultModeGate::with_mode(E::Auto);
    let input = serde_json::json!({
        "command": "curl https://one.example/a https://two.example/b"
    });
    let request = ToolRequest {
        tool_name: "bash",
        input: Some(&input),
        is_destructive: true,
        is_read_only: false,
        native_requires_approval: true,
    };
    let reason = match gate.decide(&request) {
        Decision::Ask(reason) => AskReason::from(&reason),
        other => panic!("expected origin approval: {other:?}"),
    };
    let scopes = match durable_grant("bash", &input, Some(&reason), UNFENCED, &[]) {
        DurableGrant::Network(scopes) => scopes,
        _ => panic!("expected origin scopes"),
    };
    assert_eq!(
        scopes,
        ["curl https://one.example", "curl https://two.example"]
    );
    let options = approval_options("bash", &input, Some(&reason), UNFENCED, &[]);
    assert!(options.iter().any(|o| o.option_id == "allow_always"));
    for scope in scopes {
        gate.add_egress_allow(&scope);
    }
    assert_eq!(gate.decide(&request).outcome(), Outcome::Allow);
}

/// The remember option for a directory ask names the directory the approval
/// would install, and that directory comes from the grant rather than from
/// the reason's sentence. The call's own path names a different directory, so
/// a card that read either the prose or the input back instead of the grant
/// would print the wrong one.
#[test]
fn test_remember_names_grant_dir() {
    let input = serde_json::json!({
        "path": "/work/repo/wiki/review/bug-log.md"
    });
    let grants = [BoundaryGrant {
        directory: PathBuf::from("/data/granted"),
        access: BoundaryAccess::ReadWrite,
    }];
    let reason = AskReason {
        source: AskSource::Detection,
        validator: "path-bounds".to_string(),
        detail: "path outside the workspace; approval authorizes parent directory /work/repo/wiki/review with read-write access".into(),
        containment_note: None,
    };
    assert!(matches!(
        durable_grant("write", &input, Some(&reason), FENCED, &grants),
        DurableGrant::Directories
    ));
    let options = approval_options("write", &input, Some(&reason), FENCED, &grants);
    let always = options
        .iter()
        .find(|o| o.option_id == "allow_always")
        .expect("allow_always option present");
    assert_eq!(always.name, "/data/granted (this workspace)");
    assert_eq!(always.scope, Some(ApprovalScope::Workspace));

    // A client's choice can shorten the lifetime the ask offers, never lengthen
    // it: a workspace answer on a session-scoped plan keeps the session, and a
    // workspace answer on a plan with no durable grant keeps nothing.
    let session_plan = DurableGrant::Network(vec!["api.github.com *".into()]);
    assert_eq!(
        installed_scope(&session_plan, ApprovalScope::Workspace),
        ApprovalScope::Session,
        "a workspace answer cannot outlive a session-scoped plan"
    );
    assert_eq!(
        installed_scope(&session_plan, ApprovalScope::Once),
        ApprovalScope::Once,
        "a once answer keeps nothing"
    );
    assert_eq!(
        installed_scope(&DurableGrant::Once, ApprovalScope::Workspace),
        ApprovalScope::Once,
        "a plan with no durable grant keeps nothing at any choice"
    );

    // An ask with no grant offers no remember option: the card cannot promise
    // a scope the durable store would never receive.
    assert!(matches!(
        durable_grant("write", &input, Some(&reason), FENCED, &[]),
        DurableGrant::Once
    ));
    let ungranted = approval_options("write", &input, Some(&reason), FENCED, &[]);
    let ids: Vec<&str> = ungranted.iter().map(|o| o.option_id.as_str()).collect();
    assert_eq!(ids, ["allow_once", "reject_once"]);
}

/// Two grants are both named on the card: the remember option installs every
/// grant, so a label naming one would promise less than the approval grants.
#[test]
fn test_remember_names_both_dirs() {
    let input = serde_json::json!({"path": "/data/a", "pattern": "/data/b/**/*.rs"});
    let grants = [
        BoundaryGrant {
            directory: PathBuf::from("/data/a"),
            access: BoundaryAccess::ReadOnly,
        },
        BoundaryGrant {
            directory: PathBuf::from("/data/b"),
            access: BoundaryAccess::ReadOnly,
        },
    ];
    let reason = AskReason {
        source: AskSource::Detection,
        validator: "path-bounds".to_string(),
        detail: "path outside the workspace".into(),
        containment_note: None,
    };
    let options = approval_options("glob", &input, Some(&reason), UNFENCED, &grants);
    let always = options
        .iter()
        .find(|o| o.option_id == "allow_always")
        .expect("allow_always option present");
    assert_eq!(always.name, "/data/a, /data/b (this workspace)");
    assert_eq!(always.scope, Some(ApprovalScope::Workspace));
}

/// A recovered ask carries no gate reason, so the disclosure writes one from
/// the grants. That sentence mentions a directory as well; the remember
/// option must still name the granted directory instead of a fragment of the
/// sentence the card itself wrote. The sentence here names a decoy directory,
/// so reading it back would print the wrong one.
#[test]
fn test_recovered_ask_names_dir() {
    let input = serde_json::json!({"path": "/data/outside/report.txt"});
    let grants = [BoundaryGrant {
        directory: PathBuf::from("/data/outside"),
        access: BoundaryAccess::ReadOnly,
    }];
    let reason = AskReason {
        source: AskSource::Detection,
        validator: "directory-capability".to_string(),
        detail: "external path requires a sandbox directory capability; approval also authorizes read-only directory /data/decoy for this session; choosing always persists it".into(),
        containment_note: None,
    };
    let options = approval_options("read", &input, Some(&reason), UNFENCED, &grants);
    let always = options
        .iter()
        .find(|o| o.option_id == "allow_always")
        .expect("the disclosed directory can be remembered");
    assert_eq!(always.name, "/data/outside (this workspace)");
    assert_eq!(always.scope, Some(ApprovalScope::Workspace));
}

/// Every durable turn-event kind maps to a session update, an ACPX
/// notification, or neither. Streaming deltas are transient runtime events;
/// their durable mapping is the authoritative AssistantMessage at turn end.
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
                input: Value::Null,
            },
            true,
            false,
        ),
        (SessionEvent::tool_result("c", Value::Null), true, false),
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
                pre_tokens: 0,
                post_tokens: 0,
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
                validator: None,
                reason: None,
            },
            false,
            true,
        ),
        // Unknown lands on neither stream: a future binary's event type the
        // current binary does not recognize carries no mapping.
        (SessionEvent::Unknown, false, false),
        (
            SessionEvent::ModelStepTiming {
                turn: 1,
                step: 0,
                total_ms: 100,
                ttft_ms: None,
                decode_ms: None,
                reasoning_ms: None,
                response_ms: None,
            },
            false,
            false,
        ),
        (SessionEvent::ContextCleared { prior_turn: 1 }, false, false),
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
    let kind = SessionEvent::tool_result("toolu_1", Value::String("ok".into()));
    let update = map_session_update(&kind).expect("tool result maps");
    let SessionUpdate::ToolCallUpdate(upd) = update else {
        panic!("tool result is a tool-call update");
    };
    assert_eq!(upd.tool_call_id.0, "toolu_1");
    assert_eq!(upd.fields.status, Some(ToolCallStatus::Completed));
    assert_eq!(upd.fields.raw_output, Some(Value::String("ok".into())));
}

#[test]
fn test_permission_decision_maps_acpx() {
    let kind = SessionEvent::PermissionDecision {
        call_id: "c".into(),
        tool: "bash".into(),
        verdict: PermissionVerdict::Denied,
        scope: "session".into(),
        validator: None,
        reason: None,
    };
    let n = map_acpx_notification(&kind).expect("verdict maps");
    assert_eq!(n.method, AcpxMethod::ContextPermissionDecision);
    assert_eq!(n.params["verdict"], "denied");
    assert_eq!(n.params["scope"], "session");
}

/// The run-completion notification carries its duration in milliseconds under
/// the name the frontend reads. Each end spells that name out, so one side
/// changing it alone would drop every turn's duration and fail nothing else;
/// this pins the emitting half of the agreement.
#[test]
fn test_run_completed_maps_duration() {
    let record = SessionEvent::RunCompleted {
        ms: Some(620),
        outcome: None,
    };
    let n = map_acpx_notification(&record).expect("the record maps");
    assert_eq!(n.method, AcpxMethod::ContextRunCompleted);
    assert_eq!(n.params["ms"], 620);
}

/// A turn whose loop was never measured still reaches the frontend, and carries
/// no duration rather than a zero it would read as a span.
#[test]
fn test_run_completed_maps_unmeasured() {
    let record = SessionEvent::RunCompleted {
        ms: None,
        outcome: None,
    };
    let n = map_acpx_notification(&record).expect("the record maps");
    assert!(
        n.params["ms"].is_null(),
        "an unmeasured turn carries no span"
    );
}

#[test]
fn test_approval_maps_acp_permission() {
    let req = ApprovalRequest::new(
        "call_1".into(),
        "bash".into(),
        serde_json::json!({"cmd": "ls"}),
        None,
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
        let reply = acp_permission_response_to_decision(resp, call_id.clone());
        let d = reply.decision;
        assert_eq!(d.approved, approved, "option {option_id}");
        assert_eq!(d.call_id, "c");
        assert!(d.updated_input.is_none());
        assert!(
            reply.verdict.is_some(),
            "an answer is recorded: {option_id}"
        );
    }
}

/// A cancelled response still vetoes the call, but the user never answered,
/// so no verdict reaches the audit: the card must stay an unknown outcome
/// rather than be reported as a denial.
#[test]
fn test_cancel_denies_without_verdict() {
    let resp = RequestPermissionResponse {
        outcome: RequestPermissionOutcome::Cancelled,
        meta: None,
    };
    let reply = acp_permission_response_to_decision(resp, "c".into());
    assert!(
        !reply.decision.approved,
        "a cancelled ask must deny the tool"
    );
    assert!(
        reply.verdict.is_none(),
        "nobody answered the card, so no verdict is recorded"
    );
}

#[test]
fn test_permission_mode_both_ways() {
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

/// A recap reaches the client as its own acpx row, never as a chat message:
/// the client draws it apart from the conversation. The attempt record is
/// audit only, since the reader derives success from the recap beside it.
#[test]
fn test_recap_maps_acpx_only() {
    let kind = SessionEvent::AwayRecap {
        text: "wiring the focus report".into(),
        last_user_event: EventId::new(),
        as_of_event: EventId::new(),
        source: RecapSource::Away,
        model: "fast-1".into(),
        usage: None,
    };
    assert!(
        map_session_update(&kind).is_none(),
        "a recap is not a conversation message"
    );
    let notice = map_acpx_notification(&kind).expect("the recap maps");
    assert_eq!(notice.method, AcpxMethod::ContextRecap);
    assert_eq!(notice.params["text"], "wiring the focus report");

    let attempt = SessionEvent::RecapAttempt {
        last_user_event: EventId::new(),
        as_of_event: EventId::new(),
        source: RecapSource::Resume,
        model: "fast-1".into(),
    };
    assert!(map_session_update(&attempt).is_none());
    assert!(
        map_acpx_notification(&attempt).is_none(),
        "a started attempt has nothing to show"
    );
}

/// TurnAborted maps to the "aborted" trajectory label, a visible
/// session-update message chunk (so the host renders the boundary notice),
/// and the acpx mark that tells a reader the notice belongs to the turn it
/// interrupts rather than opening a turn of its own.
#[test]
fn test_turn_aborted_maps_label() {
    let kind = SessionEvent::TurnAborted {
        reason: "crash".into(),
    };
    assert_eq!(event_name(&kind), "aborted");
    assert_eq!(
        map_acpx_notification(&kind).map(|n| n.method),
        Some(AcpxMethod::ContextTurnInterrupted),
        "the notice carries the mark that keeps the turn whole"
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
    let e = RunError::ProviderFatal(ProviderError::Auth);
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
    let e = RunError::ProviderFatal(ProviderError::ModelNotFound("qwen3.8-max".into()));
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
    let entries = super::build_trajectory_entries(from_ref(&tool));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].duration_ms, Some(4200));
    let user = mk(SessionEvent::UserInput { text: "hi".into() });
    let entries2 = super::build_trajectory_entries(from_ref(&user));
    assert_eq!(entries2[0].duration_ms, None);

    // A recap's row carries its request time, the one figure that makes the
    // background spend visible in the audit page.
    let recap = mk(SessionEvent::AwayRecap {
        text: "where the work stands".into(),
        last_user_event: EventId::new(),
        as_of_event: EventId::new(),
        source: RecapSource::Away,
        model: "fast-1".into(),
        usage: Some(RecapUsage {
            input_tokens: Some(900),
            output_tokens: Some(40),
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            duration_ms: Some(760),
        }),
    });
    let entries3 = super::build_trajectory_entries(from_ref(&recap));
    assert_eq!(entries3[0].kind, "recap");
    assert_eq!(entries3[0].duration_ms, Some(760));
    let cost = entries3[0]
        .usage
        .as_ref()
        .expect("the recap's cost reaches the audit row");
    assert_eq!(cost.model, "fast-1");
    assert_eq!(cost.input_tokens, Some(900));
    assert_eq!(cost.output_tokens, Some(40));
    assert_eq!(
        cost.cache_read_input_tokens, None,
        "a figure the endpoint did not report stays unknown"
    );
    assert!(
        entries2[0].usage.is_none(),
        "an event the host did not bill carries no cost"
    );
}

/// A skill-tool always-allow scopes to the specific skill name + lands at
/// Local (machine-local), not a blanket repo-shared tool-level rule. One
/// approval cannot pre-authorize every future skill invocation for
/// collaborators, and a different skill re-asks.
#[test]
fn test_skill_consent_name_local() {
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
    let gate = DefaultModeGate::new();
    let rule =
        super::consent_rule_for("skill", &serde_json::json!({"skill": "deploy"})).expect("rule");
    gate.add_rule(rule).expect("saved");

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

#[test]
fn test_timing_cleared_name() {
    let t = SessionEvent::ModelStepTiming {
        turn: 1,
        step: 0,
        total_ms: 100,
        ttft_ms: None,
        decode_ms: None,
        reasoning_ms: None,
        response_ms: None,
    };
    assert_eq!(event_name(&t), "timing");
    let c = SessionEvent::ContextCleared { prior_turn: 1 };
    assert_eq!(event_name(&c), "cleared");
}

/// A fence that refuses egress cannot keep a remembered destination, so the
/// card offers no remember option: the saved entry would change nothing at
/// execution time and the kernel would overrule the answer. Without the
/// refusal the same ask still names the destination, so the option is hidden
/// by the fence fact alone.
#[test]
fn test_blocked_egress_hides_remember() {
    let gate = DefaultModeGate::with_mode(E::Auto).with_trusted_hosts();
    // Two destinations under an untrusted host: the mode asks, and the ask's
    // cause is the destination, which is the branch a remembered scope comes
    // from.
    let input = serde_json::json!({
        "command": "curl https://one.example/a https://two.example/b"
    });
    let request = ToolRequest {
        tool_name: "bash",
        input: Some(&input),
        is_destructive: true,
        is_read_only: false,
        native_requires_approval: true,
    };
    let reason = match gate.decide(&request) {
        Decision::Ask(reason) => AskReason::from(&reason),
        other => panic!("expected an egress approval: {other:?}"),
    };
    let blocked = CardFence {
        writes_confined: true,
        egress_blocked: true,
    };
    assert!(matches!(
        durable_grant("bash", &input, Some(&reason), blocked, &[]),
        DurableGrant::Once
    ));
    let blocked_options = approval_options("bash", &input, Some(&reason), blocked, &[]);
    let ids: Vec<&str> = blocked_options
        .iter()
        .map(|o| o.option_id.as_str())
        .collect();
    assert_eq!(ids, ["allow_once", "reject_once"]);

    let offered = approval_options("bash", &input, Some(&reason), FENCED, &[]);
    assert!(
        offered.iter().any(|o| o.option_id == "allow_always"),
        "an egress-permitting fence can remember the destination: {offered:?}"
    );
}

/// Every audit figure reaches the status view unchanged: a field the mapping
/// forgot would read as zero in the status pane while the durable fold holds
/// the real count.
#[test]
fn test_session_audit_maps_figures() {
    let summary = TrajectorySummary {
        approvals: ApprovalAudit {
            cards: 9,
            approved_once: 1,
            approved_session: 2,
            approved_workspace: 3,
            denied: 2,
            unknown_outcome: 1,
        },
        turns: TurnAudit {
            completed: 4,
            interrupted: 2,
            unfinished: 1,
            unknown: 3,
        },
        ..TrajectorySummary::default()
    };
    let view = map_session_audit(&summary);
    assert_eq!(view.approvals.cards, 9);
    assert_eq!(view.approvals.approved_once, 1);
    assert_eq!(view.approvals.approved_session, 2);
    assert_eq!(view.approvals.approved_workspace, 3);
    assert_eq!(view.approvals.denied, 2);
    assert_eq!(view.approvals.unknown_outcome, 1);
    assert_eq!(view.turns.completed, 4);
    assert_eq!(view.turns.interrupted, 2);
    assert_eq!(view.turns.unfinished, 1);
    assert_eq!(view.turns.unknown, 3);
}
