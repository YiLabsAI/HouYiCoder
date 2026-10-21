//! Tests for the git-checkpoint consent rule classification (session-scoped
//! for git ops, durable otherwise) + the ask-before-git toggle reply.
//! Extracted from server.rs to keep the file under the size gate.

use super::super::Server;
use crate::protocol_adapter::approval_options;
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::ModeGate;
use houyicoder_permission::{AskReason, AskSource, DefaultModeGate, Outcome, ToolRequest};
use houyicoder_protocol::envelope::ResponsePayload;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use std::sync::Arc;

/// Build a Server over a stub runner + a real gate (returned so the test
/// can inspect how a consent was recorded).
fn server_with_gate() -> (Server, Arc<DefaultModeGate>) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("test"));
    let runner = Runner::new(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    let gate = Arc::new(DefaultModeGate::new());
    let server = Server::new(Arc::new(runner), SessionId::new(), gate.clone());
    (server, gate)
}

#[test]
fn test_git_ops_query_set() {
    let (server, _gate) = server_with_gate();
    // Query: default on.
    assert!(matches!(
        server.ask_before_git_response(None),
        ResponsePayload::PermissionAskBeforeGit(true)
    ));
    // Set off; the reply carries the new state.
    assert!(matches!(
        server.ask_before_git_response(Some(false)),
        ResponsePayload::PermissionAskBeforeGit(false)
    ));
    // A follow-up query reflects the set.
    assert!(matches!(
        server.ask_before_git_response(None),
        ResponsePayload::PermissionAskBeforeGit(false)
    ));
}

/// The reason a destructive-command detection ask carries. A yes-always on it
/// installs a durable rule.
fn detection_reason() -> AskReason {
    AskReason {
        source: AskSource::Detection,
        validator: "destructive_command",
        detail: "destructive pattern needs confirmation".into(),
        containment_note: None,
    }
}

/// The reason a network ask carries.
fn network_reason() -> AskReason {
    AskReason {
        source: AskSource::Detection,
        validator: "network_egress",
        detail: "this command reaches the network".into(),
        containment_note: None,
    }
}

#[test]
fn test_git_consent_skips_rule() {
    let (server, gate) = server_with_gate();
    let durable =
        |g: &Arc<DefaultModeGate>| g.rules().iter().filter(|r| r.scope.is_writable()).count();
    // The gate seeds four builtin rules at construction; none are durable.
    assert_eq!(durable(&gate), 0, "no durable rules at start");
    // A git commit approval routes to a session-scope allow rule (in-memory,
    // not durable), shadowing the builtin ask rule this session.
    server.apply_consent_rule(
        "bash",
        &serde_json::json!({"command": "git commit -m x"}),
        Some(&detection_reason()),
    );
    assert_eq!(
        durable(&gate),
        0,
        "git op consent stays session-scoped, no durable rule"
    );
    // A non-git command takes the durable-rule path (contrast).
    server.apply_consent_rule(
        "bash",
        &serde_json::json!({"command": "npm install"}),
        Some(&detection_reason()),
    );
    assert_eq!(durable(&gate), 1, "non-git command becomes a durable rule");
}

/// A yes-always on a network ask remembers the destination for the session, so
/// the next command naming the same origin runs without a prompt.
#[test]
fn test_network_consent_remembers_host() {
    let (server, gate) = server_with_gate();
    let first = "CACERT=/opt/cacert.pem; curl -sS --cacert \"$CACERT\" \
                 \"https://api.github.com/repos/ray-project/ray/pulls/66986\" \
                 -o \"$TMPDIR/pr.json\"; python3 -c \"print(1)\"";
    let second = "CACERT=/opt/cacert.pem; curl -sS --cacert \"$CACERT\" \
                  \"https://api.github.com/repos/ray-project/ray/pulls/66986/comments\" \
                  -o \"$TMPDIR/ic.json\"; python3 -c \"print(2)\"";
    let decide = |command: &str| {
        let input: &'static serde_json::Value =
            Box::leak(serde_json::json!({"command": command}).into());
        gate.decide(&ToolRequest {
            tool_name: "bash",
            input: Some(input),
            is_destructive: true,
            is_read_only: false,
            native_requires_approval: true,
        })
        .outcome()
    };
    assert_eq!(decide(first), Outcome::Ask);
    server.apply_consent_rule(
        "bash",
        &serde_json::json!({"command": first}),
        Some(&network_reason()),
    );
    assert_eq!(decide(first), Outcome::Allow);
    assert_eq!(decide(second), Outcome::Allow);
    // A different origin is still a new decision.
    assert_eq!(decide("curl https://evil.test/x"), Outcome::Ask);
}

/// The approval card offers the remember option only when the server can
/// install the scope it names, and names the destination rather than the
/// command's first word.
#[test]
fn test_card_offers_destination_scope() {
    let command = "CACERT=/opt/cacert.pem; curl -sS --cacert \"$CACERT\" \
                   \"https://api.github.com/repos/a/b\" -o \"$TMPDIR/p.json\"; \
                   python3 -c \"print(1)\"";
    let options = approval_options(
        "bash",
        &serde_json::json!({"command": command}),
        Some(&network_reason()),
    );
    let always = options
        .iter()
        .find(|o| o.option_id == "allow_always")
        .expect("remember option offered");
    assert_eq!(always.name, "curl https://api.github.com *");

    // A command whose destination cannot be named offers no remember option.
    let options = approval_options(
        "bash",
        &serde_json::json!({"command": "curl \"$URL\""}),
        Some(&network_reason()),
    );
    assert!(
        options.iter().all(|o| o.option_id != "allow_always"),
        "no remember option for an unscopable command: {options:?}"
    );
}

/// A protected-path ask never offers a durable scope: consent cannot override
/// it.
#[test]
fn test_card_safety_hides_remember() {
    let reason = AskReason {
        source: AskSource::SystemSafety,
        validator: "protected_path",
        detail: "accessing a protected path needs confirmation".into(),
        containment_note: None,
    };
    let options = approval_options(
        "write",
        &serde_json::json!({"path": ".git/config", "content": "x"}),
        Some(&reason),
    );
    assert!(
        options.iter().all(|o| o.option_id != "allow_always"),
        "no remember option on a safety ask: {options:?}"
    );
}
