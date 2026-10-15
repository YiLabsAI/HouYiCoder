//! Tests for the git-checkpoint consent rule classification (session-scoped
//! for git ops, durable otherwise) + the ask-before-git toggle reply.
//! Extracted from server.rs to keep the file under the size gate.

use super::super::Server;
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_permission::ModeGate;
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

#[test]
fn test_git_consent_skips_rule() {
    let (server, gate) = server_with_gate();
    let durable =
        |g: &Arc<DefaultModeGate>| g.rules().iter().filter(|r| r.scope.is_writable()).count();
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
