//! Tests for the git-checkpoint consent rule classification (session-scoped
//! for git ops, durable otherwise) + the ask-before-git toggle reply.
//! Extracted from server.rs to keep the file under the size gate.

use super::Server;
use houyicoder_permission::ModeGate;
use houyicoder_protocol::envelope::ResponsePayload;
use std::sync::Arc;

/// Build a Server over a stub runner + a real gate (returned so the test
/// can inspect how a consent was recorded).
fn server_with_gate() -> (Server, Arc<houyicoder_permission::DefaultModeGate>) {
    let store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(houyicoder_provider::FakeProvider::text("test"));
    let runner = houyicoder_core::agent::Runner::with_shared_store(
        store,
        provider,
        houyicoder_core::agent::ToolRegistry::new(),
        houyicoder_core::agent::runner_config::RunnerConfig::default(),
    );
    let gate = Arc::new(houyicoder_permission::DefaultModeGate::new());
    let server = Server::new(
        Arc::new(runner),
        houyicoder_context::SessionId::new(),
        gate.clone(),
    );
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

#[test]
fn test_git_consent_skips_rule() {
    let (server, gate) = server_with_gate();
    let durable = |g: &std::sync::Arc<houyicoder_permission::DefaultModeGate>| {
        g.rules().iter().filter(|r| r.scope.is_writable()).count()
    };
    // The gate seeds four builtin rules at construction; none are durable.
    assert_eq!(durable(&gate), 0, "no durable rules at start");
    // A git commit approval routes to a session-scope allow rule (in-memory,
    // not durable), shadowing the builtin ask rule this session.
    server.apply_consent_rule("bash", &serde_json::json!({"command": "git commit -m x"}));
    assert_eq!(
        durable(&gate),
        0,
        "git op consent stays session-scoped, no durable rule"
    );
    // A non-git command takes the durable-rule path (contrast).
    server.apply_consent_rule("bash", &serde_json::json!({"command": "npm install"}));
    assert_eq!(durable(&gate), 1, "non-git command becomes a durable rule");
}
