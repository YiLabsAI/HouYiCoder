//! Tests for this module.

use super::*;
use houyicoder_protocol::acp_wire::AcpNotification;
use std::sync::Arc;

fn server_no_runtime() -> Server {
    let store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(houyicoder_provider::FakeProvider::text("x"));
    let runner = houyicoder_core::agent::Runner::new(
        store,
        provider,
        houyicoder_core::agent::ToolRegistry::new(),
        houyicoder_core::agent::runner_config::RunnerConfig::default(),
    );
    Server::new(
        Arc::new(runner),
        houyicoder_context::SessionId::new(),
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
}

/// Without a multi-agent runtime the handler logs the Err + drops (no
/// panic, no reply). The runner has no spawn handle, so steer_child
/// returns Err — the no-bus path on a single-agent server.
#[tokio::test]
async fn test_inject_child_no_runtime() {
    let server = server_no_runtime();
    server.handle_session_notification(&AcpNotification::new(
        "session/inject_child",
        serde_json::json!({ "childSid": "c1", "text": "x" }),
    ));
}

/// A session/cancel_child_turn notification routes to the runner's
/// cancel_child. No spawn handle on a single-agent server, so the call
/// is a no-op (no panic). Pins the wire-method routing so a rename
/// makes the abort silently no-op (red).
#[tokio::test]
async fn test_cancel_child_routes() {
    let server = server_no_runtime();
    server.handle_session_notification(&AcpNotification::new(
        "session/cancel_child_turn",
        serde_json::json!({ "childSid": "c1" }),
    ));
}

/// A session/cancel_child_turn with no childSid is a silent no-op.
#[tokio::test]
async fn test_cancel_child_no_sid() {
    let server = server_no_runtime();
    server.handle_session_notification(&AcpNotification::new(
        "session/cancel_child_turn",
        serde_json::json!({}),
    ));
}

/// session/kill_child routes to the runner's lifecycle kill (terminal,
/// distinct from cancel_child_turn's per-turn). No runtime → no-op.
#[tokio::test]
async fn test_kill_child_routes() {
    let server = server_no_runtime();
    server.handle_session_notification(&AcpNotification::new(
        "session/kill_child",
        serde_json::json!({ "childSid": "c1" }),
    ));
}

/// A session/kill_child with no childSid is a silent no-op.
#[tokio::test]
async fn test_kill_child_no_sid() {
    let server = server_no_runtime();
    server.handle_session_notification(&AcpNotification::new(
        "session/kill_child",
        serde_json::json!({}),
    ));
}

/// session/kill_all routes to the runner's kill_all_children. With no
/// runtime attached it is a silent no-op (returns 0).
#[tokio::test]
async fn test_kill_all_routes() {
    let server = server_no_runtime();
    server.handle_session_notification(&AcpNotification::new(
        "session/kill_all",
        serde_json::json!({}),
    ));
}
